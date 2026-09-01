//! The ways through a stream of tokens that stand side by side.
//!
//! An analyzer may leave more than one token in a place -- a stem stacked on
//! its word, a synonym beside what it means -- and a token may span several
//! places, as `wtf` does next to `what the fudge`. The tokens are then arcs of
//! a graph, and a query over the text is a query over the ways through it.

/// One token as an edge of the graph: the word, and the places it joins.
#[derive(Clone, Debug)]
pub(crate) struct Edge {
    pub text: String,
    pub from: usize,
    pub to: usize,
}

impl Edge {
    pub(crate) fn new(text: String, from: usize, length: usize) -> Edge {
        Edge { text, from, to: from + length.max(1) }
    }
}

/// Whether the stream is anything more than one word after another.
pub(crate) fn branches(arcs: &[Edge]) -> bool {
    arcs.iter().any(|a| a.to - a.from > 1)
        || arcs.windows(2).any(|pair| pair[0].from == pair[1].from)
}

/// The places where the graph can be cut without cutting an arc.
///
/// Between two cuts the ways are independent of the ways anywhere else, so a
/// query that wants every word can want each stretch in turn.
fn cuts(arcs: &[Edge], nodes: &[usize]) -> Vec<usize> {
    nodes
        .iter()
        .copied()
        .filter(|node| !arcs.iter().any(|a| a.from < *node && *node < a.to))
        .collect()
}

/// Every way from one node to another, as the words along it.
fn ways_between(arcs: &[Edge], nodes: &[usize], from: usize, to: usize) -> Vec<Vec<String>> {
    const MOST: usize = 64;
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut open: Vec<(usize, Vec<String>)> = vec![(from, Vec::new())];
    while let Some((at, walked)) = open.pop() {
        if at >= to {
            out.push(walked);
            if out.len() >= MOST {
                break;
            }
            continue;
        }
        let leaving: Vec<&Edge> = arcs.iter().filter(|a| a.from == at && a.to <= to).collect();
        if leaving.is_empty() {
            // a place nothing leaves from -- a word that was dropped -- is
            // stepped over to the next place something does
            match nodes.iter().copied().find(|n| *n > at) {
                Some(next) => open.push((next, walked)),
                None => out.push(walked),
            }
            continue;
        }
        for arc in leaving.into_iter().rev() {
            let mut longer = walked.clone();
            longer.push(arc.text.clone());
            open.push((arc.to, longer));
        }
    }
    out.retain(|way| !way.is_empty());
    out.dedup();
    out
}

/// Every way through the whole graph.
pub(crate) fn ways(arcs: &[Edge]) -> Vec<Vec<String>> {
    let nodes = nodes_of(arcs);
    match (nodes.first(), nodes.last()) {
        (Some(first), Some(last)) => ways_between(arcs, &nodes, *first, *last),
        _ => Vec::new(),
    }
}

/// The graph cut into stretches, each with every way through it.
pub(crate) fn stretches(arcs: &[Edge]) -> Vec<Vec<Vec<String>>> {
    let nodes = nodes_of(arcs);
    let cuts = cuts(arcs, &nodes);
    cuts.windows(2)
        .map(|pair| ways_between(arcs, &nodes, pair[0], pair[1]))
        .filter(|ways| !ways.is_empty())
        .collect()
}

fn nodes_of(arcs: &[Edge]) -> Vec<usize> {
    let mut nodes: Vec<usize> = arcs.iter().flat_map(|a| [a.from, a.to]).collect();
    nodes.sort_unstable();
    nodes.dedup();
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arc(text: &str, from: usize, length: usize) -> Edge {
        Edge::new(text.to_string(), from, length)
    }

    #[test]
    fn a_synonym_that_spans_words_makes_two_ways() {
        // say | what the fudge / wtf | happened
        let arcs = vec![
            arc("say", 0, 1),
            arc("what", 1, 1),
            arc("wtf", 1, 3),
            arc("the", 2, 1),
            arc("fudge", 3, 1),
            arc("happened", 4, 1),
        ];
        assert!(branches(&arcs));
        let all = ways(&arcs);
        assert_eq!(all.len(), 2);
        assert!(all.contains(&vec!["say".into(), "wtf".into(), "happened".into()]));
        let parts = stretches(&arcs);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[1].len(), 2);
    }

    #[test]
    fn a_dropped_word_leaves_a_gap_that_is_stepped_over() {
        let arcs = vec![arc("quick", 0, 1), arc("fox", 2, 1)];
        assert_eq!(ways(&arcs), vec![vec!["quick".to_string(), "fox".to_string()]]);
    }
}
