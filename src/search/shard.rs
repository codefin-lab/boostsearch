//! One index's share of a search: the query put to BoostCore, the
//! candidates it gives back, and what it collected on the way.

use super::*;

/// An aggregation collector that may not be there.
///
/// Hits and aggregations were two separate searches over the same query, which
/// meant building the weight and walking every segment twice per index. At a
/// couple of hundred indices that second pass is most of the cost, so the two
/// now ride in one collector tuple -- and a request without aggregations still
/// needs something to occupy that slot.
pub(crate) struct MaybeAgg(Option<DistributedAggregationCollector>);

pub(crate) struct MaybeAggSegment(Option<boostcore::aggregation::AggregationSegmentCollector>);

/// Run a shard's search, choosing where the per-segment work goes.
///
/// BoostCore's own `search` hands the segments to the index's shared executor.
/// When a query fans out over many indices the outer parallelism already keeps
/// every core busy, and asking that same pool for per-segment parallelism from
/// inside it means each shard queues behind the others: measured on two hundred
/// empty indices, a search that should be free took 147us of elapsed time
/// waiting. One index at a time still wants the pool -- that is where
/// per-segment parallelism pays.
pub(crate) fn search_shard<C: boostcore::collector::Collector>(
    searcher: &Searcher,
    query: &dyn boostcore::query::Query,
    collector: &C,
    fanned_out: bool,
) -> boostcore::Result<C::Fruit> {
    if !fanned_out {
        return searcher.search(query, collector);
    }
    let scoring = if collector.requires_scoring() {
        boostcore::query::EnableScoring::enabled_from_statistics_provider(searcher, searcher)
    } else {
        boostcore::query::EnableScoring::disabled_from_searcher(searcher)
    };
    searcher.search_with_executor(query, collector, &boostcore::Executor::single_thread(), scoring)
}

// One shard's work touches only its own index, so the fan-out runs across
// cores. Searching many small indices is otherwise bounded by walking them
// one at a time.
pub(crate) struct ShardOut {
    pub(crate) name: String,
    pub(crate) searcher: Searcher,
    pub(crate) st: std::sync::Arc<parking_lot::RwLock<IdxState>>,
    pub(crate) shards: u64,
    pub(crate) count: usize,
    pub(crate) cands: Vec<Cand>,
    pub(crate) agg: Option<IntermediateAggregationResults>,
    pub(crate) agg_req: Option<Aggregations>,
    pub(crate) agg_meta: Vec<(String, Value)>,
    pub(crate) bucket_orders: Vec<(String, String, bool)>,
    pub(crate) profile: Option<Value>,
}

/// Search one index, as one shard of the whole request.
#[allow(clippy::too_many_arguments)]
pub(crate) fn search_one_shard(
    store: &Store,
    shard_idx: usize,
    name: &str,
    body: &Value,
    p: &Params,
    query_json: &Option<Value>,
    sort_keys: &[SortKey],
    search_after: &Option<Vec<SortValue>>,
    pit_ceiling: &std::collections::HashMap<String, u64>,
    agg_json: &Option<Value>,
    filters_aggs: &[(String, Value)],
    page_want: usize,
    fanned_out: bool,
) -> std::result::Result<Option<ShardOut>, Response> {
    let Some(st) = store.get(name) else { return Ok(None) };
    let g = st.read();
    g.search_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if p.get("request_cache").map(|v| v == "true").unwrap_or(false) {
        g.request_cache_miss.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let mut shards = 0u64;
    let mut cands: Vec<Cand> = Vec::new();
    let mut agg_acc: Option<IntermediateAggregationResults> = None;
    let mut agg_req: Option<Aggregations> = None;
    let mut agg_meta: Vec<(String, Value)> = Vec::new();
    let mut bucket_orders: Vec<(String, String, bool)> = Vec::new();
    shards += g.shard_count();
    let ctx = Ctx {
        fields: &g.fields,
        mapping: &g.mapping,
        index: &g.index,
        max_terms_count: g.max_terms_count(),
        max_regex_length: g.max_regex_length(),
        allow_expensive: crate::search::expensive_allowed(store),
        observed_kinds: &g.observed_kinds,
        kinds_complete: g.kinds_complete,
        stats: &g.stats,
    };
    let q: Box<dyn boostcore::query::Query> = match &query_json {
        Some(qj) => match crate::query::build(&ctx, qj) {
            Ok(q) => q,
            Err(e) => {
                return Err(err(StatusCode::BAD_REQUEST, "parsing_exception", e.to_string()));
            }
        },
        None => Box::new(boostcore::query::AllQuery),
    };
    // a point in time holds the search to what the index had written when
    // it was opened, which is what makes paging through it stable
    let q: Box<dyn boostcore::query::Query> = match pit_ceiling.get(name) {
        Some(ceiling) => {
            let upper = boostcore::Term::from_field_u64(g.fields.seq, *ceiling);
            let below = boostcore::query::FastFieldRangeQuery::new(
                std::ops::Bound::Unbounded,
                std::ops::Bound::Excluded(upper),
            );
            Box::new(boostcore::query::BooleanQuery::new(vec![
                (boostcore::query::Occur::Must, q),
                (
                    boostcore::query::Occur::Must,
                    Box::new(below) as Box<dyn boostcore::query::Query>,
                ),
            ]))
        }
        None => q,
    };

    let searcher = g.reader.searcher();

    // the peeled aggregations never reach the parser, so their fields are
    // checked here rather than alongside the ones that do
    if !filters_aggs.is_empty() {
        let peeled: Value = filters_aggs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<serde_json::Map<_, _>>()
            .into();
        check_agg_types(&peeled, &ctx)?;
    }

    // aggregations, when asked for, run over the same query
    let mut this_agg: Option<Aggregations> = None;
    let mut agg_request_json: Option<Value> = None;
    if let Some(aj) = &agg_json {
        let mut rewritten = aj.clone();
        normalize_aggs(&mut rewritten, &mut agg_meta, true);
        check_agg_types(&rewritten, &ctx)?;
        normalize_agg_dates(&mut rewritten);
        bucket_orders = extract_bucket_orders(&mut rewritten);
        let _ = extract_partitions(&mut rewritten);
        lower_nested_filters(&mut rewritten, &ctx);
        strip_untranslatable_term_filters(&mut rewritten, &ctx);
        // before the fields are renamed to the columns they live in, so
        // the mapping still answers for the name the request used
        fixed_date_histograms(&mut rewritten, &ctx);
        rewrite_agg_fields(&mut rewritten, &ctx);
        agg_request_json = Some(rewritten.clone());
        match serde_json::from_value::<Aggregations>(rewritten) {
            Ok(a) => this_agg = Some(a),
            Err(e) => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "x_content_parse_exception",
                    format!("failed to parse aggregation: {e}"),
                ));
            }
        }
    }

    let want = page_want;
    // The aggregation rides along with the hit collection so the query is
    // walked once per index rather than twice. Profiling drives the phases
    // itself and keeps its own pass.
    let profiling = body.get("profile").map(|v| v == true).unwrap_or(false);
    let agg_collector = MaybeAgg(match (&this_agg, profiling) {
        (Some(a), false) => {
            let ctxp = AggContextParams::new(Default::default(), g.index.tokenizers().clone());
            Some(DistributedAggregationCollector::from_aggs(a.clone(), ctxp))
        }
        _ => None,
    });

    let searched = if want == 0 {
        // `size: 0` asks for counts and aggregations only. Collecting a
        // page anyway means scoring and heap-ordering every match for a
        // result that is thrown away.
        search_shard(&searcher, &q, &(Count, agg_collector), fanned_out)
            .map(|(c, agg)| (c, Vec::new(), agg))
    } else if sort_keys.is_empty() && agg_collector.0.is_none() && count_without_walking(query_json)
    {
        // Nothing else needs every document, so the top-k collector can
        // prune: once its heap is full, whole blocks that cannot beat the
        // worst kept score are skipped. Bundling a counter alongside it
        // would force every document to be visited and give that up --
        // measured at three to four times the throughput on this shape.
        //
        // The count then comes from the weight, which answers it from the
        // postings header for the queries that can, and otherwise walks
        // the same documents the tuple would have.
        // This query is cheap: the heap prunes and only `want` documents
        // are kept. Splitting its segments across the pool costs more in
        // coordination than the walk itself, and steals cores from the
        // aggregations, which are the expensive shape and do need them.
        let topk =
            search_shard(&searcher, &q, &TopDocs::with_limit(want.max(1)).order_by_score(), true);
        topk.and_then(|docs| {
            let cands = docs
                .into_iter()
                .map(|(score, addr)| Cand {
                    shard: shard_idx,
                    addr,
                    score,
                    sort: Vec::new(),
                    seq: u64::MAX,
                })
                .collect::<Vec<_>>();
            let count = count_matches(&searcher, &q)?;
            Ok((count, cands, None))
        })
    } else if sort_keys.is_empty() {
        // an aggregation needs every document anyway, so there is nothing
        // to prune and hits ride along in the same pass
        let collector = (Count, TopDocs::with_limit(want.max(1)).order_by_score(), agg_collector);
        search_shard(&searcher, &q, &collector, fanned_out).map(|(c, docs, agg)| {
            let cands = docs
                .into_iter()
                .map(|(score, addr)| Cand {
                    shard: shard_idx,
                    addr,
                    score,
                    sort: Vec::new(),
                    seq: u64::MAX,
                })
                .collect::<Vec<_>>();
            (c, cands, agg)
        })
    } else {
        // sort keys are evaluated during collection, so only `want`
        // candidates are ever held rather than one per match
        let sources: Vec<SortSource> = sort_keys
            .iter()
            .map(|k| match k.field.as_str() {
                "_score" => SortSource::Score,
                "_doc" => SortSource::Doc,
                // `_seq` is a column of the index itself, not a field
                // inside either JSON view, so it is named as it is
                "_seq" => SortSource::Column {
                    name: "_seq".to_string(),
                    desc: k.desc,
                    mode: k.mode.clone(),
                },
                // The values of a field inside a nested object belong to
                // the object, not to the document, so a sort that does not
                // say which object it reads inside finds nothing -- which
                // is what OpenSearch's resolveNested returning null means.
                _ if k.nested.is_none() && under_nested(ctx.mapping, &k.field) => {
                    SortSource::Column {
                        name: "_bs_no_such_column".to_string(),
                        desc: k.desc,
                        mode: k.mode.clone(),
                    }
                }
                // a date is a number in the index -- milliseconds, or
                // nanoseconds for a date_nanos -- which is the number
                // OpenSearch reports, so nothing is rescaled
                _ => SortSource::Column {
                    name: ctx.column_name(&k.field, false),
                    desc: k.desc,
                    mode: k.mode.clone(),
                },
            })
            .collect();
        let desc: Vec<bool> = sort_keys.iter().map(|k| k.desc).collect();
        let collector = (
            Count,
            SortCollector {
                sources,
                missing_last: sort_keys.iter().map(|k| k.missing_last).collect(),
                desc,
                limit: want.max(1),
                after: search_after.clone(),
            },
            agg_collector,
        );
        search_shard(&searcher, &q, &collector, fanned_out).map(|(c, mut cands, agg)| {
            for cand in cands.iter_mut() {
                cand.shard = shard_idx;
            }
            (c, cands, agg)
        })
    };
    let (count, shard_cands, shard_agg) = match searched {
        Ok(v) => v,
        Err(e) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "search_phase_execution_exception",
                e.to_string(),
            ));
        }
    };
    if let Some(res) = shard_agg {
        agg_acc = Some(res);
        agg_req = this_agg.clone();
    }
    cands.extend(shard_cands);

    let mut shard_profile = None;
    // a profile is asked for by the request, not by the aggregations: a
    // search with no aggregations still has a shard to report on
    if profiling && this_agg.is_none() {
        shard_profile = Some(json!({
            "id": "[boostsearch][0]",
            "searches": [],
            "aggregations": [],
        }));
    }
    if let (Some(a), true) = (this_agg, profiling) {
        let ctxp = AggContextParams::new(Default::default(), g.index.tokenizers().clone());
        let (res, prof) =
            profiled_agg_search(&searcher, &q, a.clone(), ctxp, &ctx, agg_request_json.as_ref());
        shard_profile = Some(prof);
        match res {
            Ok(res) => {
                agg_acc = Some(res);
                agg_req = Some(a);
            }
            Err(e) => {
                return Err(err(
                    StatusCode::BAD_REQUEST,
                    "aggregation_execution_exception",
                    e.to_string(),
                ));
            }
        }
    }

    Ok(Some(ShardOut {
        name: g.name.clone(),
        searcher,
        st: st.clone(),
        shards,
        count,
        cands,
        agg: agg_acc,
        agg_req,
        agg_meta,
        bucket_orders,
        profile: shard_profile,
    }))
}

impl boostcore::collector::Collector for MaybeAgg {
    type Fruit = Option<IntermediateAggregationResults>;
    type Child = MaybeAggSegment;

    fn for_segment(
        &self,
        ord: boostcore::SegmentOrdinal,
        reader: &boostcore::SegmentReader,
    ) -> boostcore::Result<Self::Child> {
        Ok(MaybeAggSegment(match &self.0 {
            Some(c) => Some(c.for_segment(ord, reader)?),
            None => None,
        }))
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(
        &self,
        segment_fruits: Vec<Option<boostcore::Result<IntermediateAggregationResults>>>,
    ) -> boostcore::Result<Self::Fruit> {
        let Some(inner) = &self.0 else { return Ok(None) };
        let present: Vec<boostcore::Result<IntermediateAggregationResults>> =
            segment_fruits.into_iter().flatten().collect();
        if present.is_empty() {
            return Ok(None);
        }
        inner.merge_fruits(present).map(Some)
    }
}
impl boostcore::collector::SegmentCollector for MaybeAggSegment {
    type Fruit = Option<boostcore::Result<IntermediateAggregationResults>>;

    fn collect(&mut self, doc: boostcore::DocId, score: boostcore::Score) {
        if let Some(c) = &mut self.0 {
            c.collect(doc, score);
        }
    }

    /// Forwarding this matters: BoostCore's aggregation collects a block at a
    /// time, and the default implementation would unroll it back into one call
    /// per document.
    fn collect_block(&mut self, docs: &[boostcore::DocId]) {
        if let Some(c) = &mut self.0 {
            c.collect_block(docs);
        }
    }

    fn harvest(self) -> Self::Fruit {
        self.0.map(|c| c.harvest())
    }
}
