use boostcore::query::{AutomatonWeight, EnableScoring, Query, Weight};
use boostcore::schema::*;
use boostcore::{Index, TantivyDocument, collector::Count};
use boostcore_fst::Regex;
use std::sync::Arc;

#[derive(Clone)]
struct JQ {
    field: Field,
    re: Arc<Regex>,
    path: Vec<u8>,
}
impl std::fmt::Debug for JQ {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "JQ")
    }
}
impl Query for JQ {
    fn weight(&self, _s: EnableScoring<'_>) -> boostcore::Result<Box<dyn Weight>> {
        Ok(Box::new(AutomatonWeight::<Regex>::new_for_json_path(
            self.field,
            self.re.clone(),
            &self.path,
        )))
    }
}

fn main() -> boostcore::Result<()> {
    let mut sb = Schema::builder();
    let f = sb.add_json_field(
        "_dyn",
        JsonObjectOptions::default().set_expand_dots_enabled().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("default")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        ),
    );
    let schema = sb.build();
    let index = Index::create_in_ram(schema);
    let mut w = index.writer(30_000_000)?;
    for t in ["brown fox jump", "brown emu jump", "dog"] {
        let obj: std::collections::BTreeMap<String, OwnedValue> =
            [("my_field1".to_string(), OwnedValue::from(serde_json::json!(t)))]
                .into_iter()
                .collect();
        let mut d = TantivyDocument::default();
        d.add_object(f, obj);
        w.add_document(d)?;
    }
    w.commit()?;
    let s = index.reader()?.searcher();

    let base = Term::from_field_json_path(f, "my_field1", true);
    let path = base.serialized_value_bytes().to_vec();
    println!("path bytes = {path:?}");
    // what does a real term look like?
    let mut t2 = Term::from_field_json_path(f, "my_field1", true);
    t2.append_type_and_str("fox");
    println!("term bytes = {:?}", t2.serialized_value_bytes());

    let mut pfx = Term::from_field_json_path(f, "my_field1", true);
    pfx.append_type_and_str("");
    let prefix_bytes = pfx.serialized_value_bytes().to_vec();
    let esc: String = prefix_bytes.iter().map(|b| format!("\\x{b:02x}")).collect();
    println!("prefix regex = {esc}");
    for pat in [format!("{esc}fo.*"), format!("{esc}fox"), format!("{esc}f.*")] {
        let pat = pat.as_str();
        match Regex::new(pat) {
            Ok(re) => {
                let q = JQ { field: f, re: Arc::new(re), path: path.clone() };
                println!("{pat:>12} -> {:?}", s.search(&q, &Count));
            }
            Err(e) => println!("{pat:>12} -> regex error: {e}"),
        }
    }
    Ok(())
}
