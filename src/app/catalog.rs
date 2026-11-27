//! Catalog rows (Home and search), the Discover grid, and the order rows and addons are shown in.

use stremflix_rs::addon::{CatalogDef, MetaPreview};

/// One horizontal catalog row on Home or the search page; empty rows are hidden.
pub(crate) struct Row {
    pub(crate) title: String,
    pub(crate) src: (String, String, String),
    pub(crate) page: Paged,
    /// Why the first page failed; a Home row shows it with a Retry.
    pub(crate) error: Option<String>,
}

/// A catalog's items, loaded a page at a time through the `skip` extra.
#[derive(Default)]
pub(crate) struct Paged {
    /// `None` while the first page loads.
    pub(crate) items: Option<Vec<MetaPreview>>,
    /// The catalog declares the `skip` extra.
    pub(crate) skippable: bool,
    /// Items received so far (the next `skip`), before de-duplication.
    pub(crate) fetched: usize,
    pub(crate) loading_more: bool,
    /// The last page was empty, had nothing new, or failed.
    pub(crate) exhausted: bool,
}

impl Paged {
    pub(crate) fn new(skippable: bool) -> Self {
        Self { skippable, ..Default::default() }
    }

    pub(crate) fn first(&mut self, items: Vec<MetaPreview>) {
        *self = Self { fetched: items.len(), items: Some(items), ..Self::new(self.skippable) };
    }

    /// The `skip` of the next page to ask for once scrolled near the end, marked as in flight.
    pub(crate) fn next_page(&mut self, near_end: bool) -> Option<usize> {
        let can = self.skippable && !self.loading_more && !self.exhausted && self.items.as_ref().is_some_and(|i| !i.is_empty() && i.len() < ROW_CAP);
        self.loading_more |= near_end && can;
        (near_end && can).then_some(self.fetched)
    }

    /// The page asked for at `skip`: appended without repeats. `None` when it is not the awaited page (stale);
    /// an error ends the paging and is handed back.
    pub(crate) fn on_page(&mut self, skip: usize, res: Result<Vec<MetaPreview>, String>) -> Option<Result<(), String>> {
        if !self.loading_more || self.fetched != skip {
            return None;
        }
        self.loading_more = false;
        Some(match res {
            Ok(page) => {
                self.fetched += page.len();
                self.exhausted = !append_page(self.items.get_or_insert_with(Vec::new), page);
                Ok(())
            }
            Err(e) => {
                self.exhausted = true;
                Err(e)
            }
        })
    }
}

/// What the search results page shows.
pub(crate) struct SearchPage {
    pub(crate) query: String,
    /// One row per searched catalog; empty rows are hidden.
    pub(crate) rows: Vec<Row>,
    /// Errors from the catalogs that failed; only reported if all of them did.
    pub(crate) failed: Vec<String>,
}

/// Row title of an addon's catalog.
pub(crate) fn catalog_title(addon: &str, c: &CatalogDef) -> String {
    let (name, kind) = (c.name.as_deref().unwrap_or(&c.id), kind_label(&c.kind));
    let suffix = if name.to_lowercase().contains(&kind.to_lowercase()) { String::new() } else { format!(" ({kind})") };
    format!("{addon} - {name}{suffix}")
}

/// Stable id of a Home row: `<addon url>|<type>|<catalog id>`.
pub(crate) fn row_key(src: &(String, String, String)) -> String {
    format!("{}|{}|{}", src.0, src.1, src.2)
}

/// Indices into `keys` with the keys in `order` first (unknown or repeated ones skipped), the rest in their own order.
pub(crate) fn arrange_rows(keys: &[String], order: &[String]) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    for k in order {
        if let Some(i) = keys.iter().position(|x| x == k)
            && !out.contains(&i)
        {
            out.push(i);
        }
    }
    let rest: Vec<usize> = (0..keys.len()).filter(|i| !out.contains(i)).collect();
    out.extend(rest);
    out
}

/// `urls` (global priority) with those in `order` first; stale ids in `order` are ignored, unlisted urls keep global order after.
pub(crate) fn order_urls(urls: Vec<String>, order: &[String]) -> Vec<String> {
    arrange_rows(&urls, order).into_iter().map(|i| urls[i].clone()).collect()
}

/// Max items kept per catalog row.
pub(crate) const ROW_CAP: usize = 1000;

/// Addon URL, type and id of an addon catalog.
pub(crate) type AddonCatalogSrc = (String, String, String);

/// Addon URL, type, catalog id, picked extras (name, value).
pub(crate) type DiscoverSrc = (String, String, String, Vec<(String, String)>);

/// Discover tab pickers and the grid they loaded.
#[derive(Default)]
pub(crate) struct Discover {
    pub(crate) kind: String,
    /// Addon URL and catalog id.
    pub(crate) cat: (String, String),
    pub(crate) extras: Vec<(String, String)>,
    pub(crate) src: Option<DiscoverSrc>,
    pub(crate) page: Paged,
}

/// Add a catalog page to `items`, skipping ids already there and stopping at `ROW_CAP`. False when it added nothing.
pub(crate) fn append_page(items: &mut Vec<MetaPreview>, page: Vec<MetaPreview>) -> bool {
    let mut seen: std::collections::HashSet<String> = items.iter().map(|m| m.id.clone()).collect();
    let before = items.len();
    items.extend(page.into_iter().filter(|m| seen.insert(m.id.clone())).take(ROW_CAP - before));
    items.len() > before
}

pub(crate) fn kind_label(kind: &str) -> &str {
    match kind {
        "movie" => "Movies",
        "series" => "Shows",
        k => k,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_are_asked_for_only_when_they_can_exist() {
        let m = |id: &str| MetaPreview { id: id.into(), kind: "movie".into(), name: id.into(), ..Default::default() };
        let loaded = |skippable, items: Vec<MetaPreview>| {
            let mut p = Paged::new(skippable);
            p.first(items);
            p
        };
        assert_eq!(Paged::new(true).next_page(true), None, "still loading");
        assert_eq!(loaded(true, vec![m("a")]).next_page(false), None, "not near the end");
        assert_eq!(loaded(false, vec![m("a")]).next_page(true), None, "no skip extra");
        assert_eq!(loaded(true, vec![]).next_page(true), None, "an empty catalog has no next page");
        assert_eq!(loaded(true, (0..ROW_CAP).map(|i| m(&i.to_string())).collect()).next_page(true), None, "capped");
        let mut p = loaded(true, vec![m("a")]);
        assert_eq!(p.next_page(true), Some(1));
        assert_eq!(p.next_page(true), None, "in flight");
        assert_eq!(p.on_page(1, Err("down".into())), Some(Err("down".into())));
        assert_eq!(p.next_page(true), None, "exhausted after a failure");
    }

    #[test]
    fn per_type_addon_order() {
        let k = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let global = k(&["a", "b", "c"]);
        assert_eq!(order_urls(global.clone(), &k(&["c", "a", "b"])), k(&["c", "a", "b"]), "per-type order applied");
        assert_eq!(order_urls(global.clone(), &[]), global, "type without an order follows global");
        assert_eq!(order_urls(global.clone(), &k(&["gone", "b"])), k(&["b", "a", "c"]), "stale ids ignored");
        assert_eq!(order_urls(k(&["a", "b", "c", "new"]), &k(&["c", "a", "b"])), k(&["c", "a", "b", "new"]), "new addons appended");
    }

    #[test]
    fn home_rows_order_and_keys() {
        let k = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let keys = k(&["a", "b", "c", "d"]);
        assert_eq!(arrange_rows(&keys, &k(&["c", "zz", "a", "c"])), [2, 0, 1, 3], "stale and repeated keys ignored, rest keep priority");
        assert_eq!(arrange_rows(&keys, &[]), [0, 1, 2, 3], "no per-type order falls back to the global one");
        assert_eq!(row_key(&("u".into(), "movie".into(), "top".into())), "u|movie|top");
    }
}
