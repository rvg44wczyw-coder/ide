//! Pure logic behind the File Structure popup (`⌘F12`,
//! `docs/features/file-structure-and-breadcrumbs.md` §2.3/§3.1) -- no
//! `IdeApp` dependency, same shape as `editor::git_gutter`.

use ide_lsp::Symbol;

/// One row the popup lists this frame: which `symbols` entry, and how far
/// to indent it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStructureRow {
    pub symbol_index: usize,
    pub depth: usize,
}

/// Nesting depth for each entry in `symbols` (`0` at the top level),
/// relying entirely on the pre-order/range-nesting invariant
/// `ide_lsp::symbols_containing`'s own doc comment establishes: a parent
/// always precedes its descendants, and a parent's range always spans
/// every descendant's. A single left-to-right pass with a stack of
/// currently-open ranges' end positions -- pop while the top has already
/// closed before the current symbol starts, the remaining stack length is
/// this symbol's depth, then push the current symbol's own end. O(n), not
/// the O(n²) an all-pairs containment check would cost.
///
/// Not meaningful (and not used) on a `Vec<Symbol>` that isn't already in
/// this pre-order shape -- e.g. a `workspace/symbol` result, which has no
/// such guarantee.
pub fn symbol_depths(symbols: &[Symbol]) -> Vec<usize> {
    let mut open_ends: Vec<(u32, u32)> = Vec::new();
    let mut depths = Vec::with_capacity(symbols.len());
    for symbol in symbols {
        let start = (
            symbol.location.range.start.line,
            symbol.location.range.start.character,
        );
        while let Some(&end) = open_ends.last() {
            if start <= end {
                break;
            }
            open_ends.pop();
        }
        depths.push(open_ends.len());
        let end = (
            symbol.location.range.end.line,
            symbol.location.range.end.character,
        );
        open_ends.push(end);
    }
    depths
}

/// What the File Structure popup actually lists this frame
/// (`file-structure-and-breadcrumbs.md` §3.1). An empty `query` returns
/// every symbol in its natural declaration order, indented by
/// `symbol_depths` -- the file's outline. A non-empty `query` instead
/// fuzzy-filters by name (`ide_core::fuzzy_score`), sorted by score
/// descending with ties broken by original order (a stable sort), every
/// row at `depth: 0` -- filtering breaks the tree shape, so v1 doesn't try
/// to preserve indentation for a filtered result (the same "search
/// results are flat" precedent Search Everywhere's own Actions tab
/// already set).
pub fn visible_rows(symbols: &[Symbol], query: &str) -> Vec<FileStructureRow> {
    if query.is_empty() {
        let depths = symbol_depths(symbols);
        return depths
            .into_iter()
            .enumerate()
            .map(|(symbol_index, depth)| FileStructureRow {
                symbol_index,
                depth,
            })
            .collect();
    }
    let mut scored: Vec<(i64, usize)> = symbols
        .iter()
        .enumerate()
        .filter_map(|(index, symbol)| {
            ide_core::fuzzy_score(query, &symbol.name).map(|m| (m.score, index))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored
        .into_iter()
        .map(|(_, symbol_index)| FileStructureRow {
            symbol_index,
            depth: 0,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ide_lsp::{Location, Position, Range, SymbolKind};
    use std::path::PathBuf;

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    fn symbol(name: &str, kind: SymbolKind, start: Position, end: Position) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind,
            container_name: None,
            location: Location {
                path: PathBuf::from("/f.rs"),
                range: Range { start, end },
            },
        }
    }

    #[test]
    fn symbol_depths_is_empty_for_no_symbols() {
        assert!(symbol_depths(&[]).is_empty());
    }

    #[test]
    fn symbol_depths_flat_siblings_are_all_zero() {
        let symbols = vec![
            symbol("a", SymbolKind::Function, pos(0, 0), pos(1, 0)),
            symbol("b", SymbolKind::Function, pos(2, 0), pos(3, 0)),
        ];
        assert_eq!(symbol_depths(&symbols), vec![0, 0]);
    }

    #[test]
    fn symbol_depths_nested_chain_increments() {
        // Foo (0-10) contains bar (2-8) contains baz (4-6): pre-order,
        // depth-first, exactly as `flatten_document_symbols` would emit it.
        let symbols = vec![
            symbol("Foo", SymbolKind::Class, pos(0, 0), pos(10, 0)),
            symbol("bar", SymbolKind::Method, pos(2, 0), pos(8, 0)),
            symbol("baz", SymbolKind::Variable, pos(4, 0), pos(6, 0)),
        ];
        assert_eq!(symbol_depths(&symbols), vec![0, 1, 2]);
    }

    #[test]
    fn symbol_depths_closes_an_ancestor_before_a_later_sibling() {
        // Foo (0-10) contains bar (1-3); qux (5-7) is a top-level sibling
        // of Foo that starts after bar's ancestor chain has fully closed.
        let symbols = vec![
            symbol("Foo", SymbolKind::Class, pos(0, 0), pos(4, 0)),
            symbol("bar", SymbolKind::Method, pos(1, 0), pos(3, 0)),
            symbol("qux", SymbolKind::Function, pos(5, 0), pos(7, 0)),
        ];
        assert_eq!(symbol_depths(&symbols), vec![0, 1, 0]);
    }

    #[test]
    fn symbol_depths_two_separate_nested_trees() {
        let symbols = vec![
            symbol("A", SymbolKind::Class, pos(0, 0), pos(2, 0)),
            symbol("a1", SymbolKind::Method, pos(0, 5), pos(1, 0)),
            symbol("B", SymbolKind::Class, pos(3, 0), pos(5, 0)),
            symbol("b1", SymbolKind::Method, pos(3, 5), pos(4, 0)),
        ];
        assert_eq!(symbol_depths(&symbols), vec![0, 1, 0, 1]);
    }

    #[test]
    fn visible_rows_empty_query_is_natural_order_with_depths() {
        let symbols = vec![
            symbol("Foo", SymbolKind::Class, pos(0, 0), pos(10, 0)),
            symbol("bar", SymbolKind::Method, pos(2, 0), pos(8, 0)),
        ];
        let rows = visible_rows(&symbols, "");
        assert_eq!(
            rows,
            vec![
                FileStructureRow {
                    symbol_index: 0,
                    depth: 0
                },
                FileStructureRow {
                    symbol_index: 1,
                    depth: 1
                },
            ]
        );
    }

    #[test]
    fn visible_rows_empty_symbols_and_empty_query_is_empty() {
        assert!(visible_rows(&[], "").is_empty());
    }

    #[test]
    fn visible_rows_query_filters_by_fuzzy_name_match() {
        let symbols = vec![
            symbol("foo_bar", SymbolKind::Function, pos(0, 0), pos(1, 0)),
            symbol("baz", SymbolKind::Function, pos(2, 0), pos(3, 0)),
        ];
        let rows = visible_rows(&symbols, "fb");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].symbol_index, 0);
        assert_eq!(rows[0].depth, 0);
    }

    #[test]
    fn visible_rows_query_ranks_a_tighter_match_first() {
        let symbols = vec![
            symbol("far_bit", SymbolKind::Function, pos(0, 0), pos(1, 0)),
            symbol("foobar", SymbolKind::Function, pos(2, 0), pos(3, 0)),
        ];
        let rows = visible_rows(&symbols, "foobar");
        assert_eq!(rows[0].symbol_index, 1);
    }

    #[test]
    fn visible_rows_query_with_no_match_is_empty() {
        let symbols = vec![symbol("baz", SymbolKind::Function, pos(0, 0), pos(1, 0))];
        assert!(visible_rows(&symbols, "zzz_nope").is_empty());
    }

    #[test]
    fn visible_rows_query_ties_keep_original_order() {
        let symbols = vec![
            symbol("ab", SymbolKind::Function, pos(0, 0), pos(1, 0)),
            symbol("ab", SymbolKind::Function, pos(2, 0), pos(3, 0)),
        ];
        let rows = visible_rows(&symbols, "ab");
        assert_eq!(rows[0].symbol_index, 0);
        assert_eq!(rows[1].symbol_index, 1);
    }
}
