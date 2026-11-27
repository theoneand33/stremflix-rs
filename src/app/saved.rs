//! The Library tab's saved-titles list: filters, sort orders, and the facts they sort by.

use stremflix_rs::addon::Meta;
use stremflix_rs::db::SavedTitle;

/// Order of the saved-titles list in the Library tab.
#[derive(PartialEq, Clone, Copy)]
pub(crate) enum LibSort {
    Added,
    Title,
    Watched,
    Rating,
    Year,
}

/// Type filter of the saved-titles list in the Library tab.
#[derive(PartialEq, Clone, Copy)]
pub(crate) enum LibKind {
    All,
    Movie,
    Series,
}

impl LibSort {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Added => "Recently added",
            Self::Title => "Title A–Z",
            Self::Watched => "Recently watched",
            Self::Rating => "Rating",
            Self::Year => "Year",
        }
    }
}

impl LibKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::All => "All types",
            Self::Movie => "Movies",
            Self::Series => "Shows",
        }
    }

    pub(crate) fn matches(self, kind: &str) -> bool {
        match self {
            Self::All => true,
            Self::Movie => kind == "movie",
            Self::Series => kind == "series",
        }
    }
}

/// Release year from free text like "2010" or "2010–2015".
pub(crate) fn parse_year(s: &str) -> Option<i32> {
    let d: String = s.chars().skip_while(|c| !c.is_ascii_digit()).take_while(char::is_ascii_digit).collect();
    d.parse().ok().filter(|y| (1800..=2200).contains(y))
}

/// Year and IMDb rating of a title's details.
pub(crate) fn saved_facts(d: &Meta) -> (Option<i32>, Option<f64>) {
    (d.release_info.as_deref().and_then(parse_year), d.imdb_rating.as_deref().and_then(|r| r.trim().parse::<f64>().ok()).filter(|r| (0.0..=10.0).contains(r)))
}

/// Case-insensitive substring match; a blank query matches everything.
pub(crate) fn title_matches(title: &str, query: &str) -> bool {
    title.to_lowercase().contains(&query.trim().to_lowercase())
}

/// Saved titles of `kind` matching `query`, newest first unless sorted: by title (case-insensitive), or best first with missing values last.
pub(crate) fn saved_view<'a>(items: &'a [SavedTitle], kind: LibKind, sort: LibSort, query: &str) -> Vec<&'a SavedTitle> {
    let mut v: Vec<&SavedTitle> = items.iter().filter(|t| kind.matches(&t.kind) && title_matches(&t.title, query)).collect();
    // Stable sorts keep the "Recently added" order among ties; `None` is the smallest, so it ends up last when reversed.
    match sort {
        LibSort::Added => {}
        LibSort::Title => v.sort_by_key(|t| t.title.to_lowercase()),
        LibSort::Watched => v.sort_by_key(|t| std::cmp::Reverse(t.last_watched)),
        LibSort::Year => v.sort_by_key(|t| std::cmp::Reverse(t.year)),
        LibSort::Rating => v.sort_by(|a, b| b.rating.unwrap_or(f64::NEG_INFINITY).total_cmp(&a.rating.unwrap_or(f64::NEG_INFINITY))),
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_view_filters_and_sorts() {
        let t = |key: &str, title: &str| SavedTitle {
            key: key.into(),
            title: title.into(),
            kind: key.split_once(':').unwrap().0.into(),
            poster: None,
            year: None,
            rating: None,
            last_watched: None,
        };
        // Newest first, as stored.
        let items = [t("series:tt2", "beta"), t("movie:tt3", "Alpha"), t("movie:tt1", "gamma")];
        let keys = |v: Vec<&SavedTitle>| v.into_iter().map(|t| t.key.clone()).collect::<Vec<_>>();
        assert_eq!(keys(saved_view(&items, LibKind::All, LibSort::Added, "")), ["series:tt2", "movie:tt3", "movie:tt1"]);
        assert_eq!(keys(saved_view(&items, LibKind::Movie, LibSort::Added, "")), ["movie:tt3", "movie:tt1"]);
        // Case-insensitive title order, and the filter holds while sorting.
        assert_eq!(keys(saved_view(&items, LibKind::All, LibSort::Title, "")), ["movie:tt3", "series:tt2", "movie:tt1"]);
        assert_eq!(keys(saved_view(&items, LibKind::Series, LibSort::Title, "")), ["series:tt2"]);
        assert!(saved_view(&items, LibKind::Series, LibSort::Added, "").len() == 1);
        // Watched, rating and year: best first, missing last, ties keep the added order.
        let items = [
            SavedTitle { last_watched: Some(5), year: Some(1999), rating: Some(6.0), ..items[0].clone() },
            items[1].clone(),
            SavedTitle { last_watched: Some(9), year: Some(2020), rating: Some(8.5), ..items[2].clone() },
        ];
        let best_first = ["movie:tt1", "series:tt2", "movie:tt3"];
        for sort in [LibSort::Watched, LibSort::Rating, LibSort::Year] {
            assert_eq!(keys(saved_view(&items, LibKind::All, sort, "")), best_first);
        }
        assert_eq!(parse_year("2010–2015"), Some(2010));
        assert_eq!(parse_year("n/a"), None);
        assert_eq!(saved_view(&[], LibKind::All, LibSort::Title, "").len(), 0);
        // Query combines with kind and sort.
        assert_eq!(keys(saved_view(&items, LibKind::All, LibSort::Title, " A ")), ["movie:tt3", "series:tt2", "movie:tt1"]);
        assert_eq!(keys(saved_view(&items, LibKind::Movie, LibSort::Added, "ALPH")), ["movie:tt3"]);
    }

    #[test]
    fn title_match_folds_case() {
        assert!(title_matches("Amélie", "AMÉL"));
        assert!(title_matches("Straße", "straße"));
        assert!(title_matches("anything", "  "));
        assert!(!title_matches("Alpha", "beta"));
    }

    #[test]
    fn saved_facts_rejects_non_finite_ratings() {
        let rated = |r: &str| serde_json::from_value::<Meta>(serde_json::json!({"id": "tt1", "type": "movie", "name": "x", "imdbRating": r})).unwrap();
        assert_eq!(saved_facts(&rated("NaN")).1, None);
        assert_eq!(saved_facts(&rated("inf")).1, None);
        assert_eq!(saved_facts(&rated("infinity")).1, None);
        assert_eq!(saved_facts(&rated("11")).1, None);
        assert_eq!(saved_facts(&rated("8.5")).1, Some(8.5));
    }
}
