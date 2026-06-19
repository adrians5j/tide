use similar::{ChangeTag, TextDiff};

#[derive(Clone, Copy, PartialEq)]
pub enum DiffKind {
    Equal,
    Del,
    Ins,
    Replace,
}

#[derive(Clone)]
pub struct DiffRow {
    pub left_no: Option<usize>,
    pub right_no: Option<usize>,
    pub left: Option<String>,
    pub right: Option<String>,
    pub kind: DiffKind,
}

/// Compute aligned side-by-side rows from old → new text. Consecutive deletes
/// and inserts are paired onto the same row (Replace) so changes line up.
pub fn compute(old: &str, new: &str) -> Vec<DiffRow> {
    let diff = TextDiff::from_lines(old, new);
    let mut rows: Vec<DiffRow> = Vec::new();
    let mut dels: Vec<(usize, String)> = Vec::new();
    let mut inss: Vec<(usize, String)> = Vec::new();
    let mut lno = 0usize;
    let mut rno = 0usize;

    let flush = |rows: &mut Vec<DiffRow>, dels: &mut Vec<(usize, String)>, inss: &mut Vec<(usize, String)>| {
        let n = dels.len().max(inss.len());
        for i in 0..n {
            let l = dels.get(i);
            let r = inss.get(i);
            let kind = match (l.is_some(), r.is_some()) {
                (true, true) => DiffKind::Replace,
                (true, false) => DiffKind::Del,
                _ => DiffKind::Ins,
            };
            rows.push(DiffRow {
                left_no: l.map(|(n, _)| *n),
                right_no: r.map(|(n, _)| *n),
                left: l.map(|(_, s)| s.clone()),
                right: r.map(|(_, s)| s.clone()),
                kind,
            });
        }
        dels.clear();
        inss.clear();
    };

    for change in diff.iter_all_changes() {
        let text = change.value().trim_end_matches('\n').to_string();
        match change.tag() {
            ChangeTag::Equal => {
                flush(&mut rows, &mut dels, &mut inss);
                lno += 1;
                rno += 1;
                rows.push(DiffRow {
                    left_no: Some(lno),
                    right_no: Some(rno),
                    left: Some(text.clone()),
                    right: Some(text),
                    kind: DiffKind::Equal,
                });
            }
            ChangeTag::Delete => {
                lno += 1;
                dels.push((lno, text));
            }
            ChangeTag::Insert => {
                rno += 1;
                inss.push((rno, text));
            }
        }
    }
    flush(&mut rows, &mut dels, &mut inss);
    rows
}
