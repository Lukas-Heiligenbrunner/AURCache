use serde::{Deserialize, Serialize};

/// One AUR package, as the search endpoint reports it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ApiPackage {
    pub name: String,
    pub version: String,
    /// The AUR's one-line summary. `None` when the package has none.
    ///
    /// Carried because the search is `by=name-desc`: a result can match on its
    /// description alone, and without it the list shows a name with no visible
    /// reason for being there. It is also what lets the client narrow a search
    /// locally as the query grows -- filtering on the name alone would drop the
    /// description matches the AUR would have kept.
    pub description: Option<String>,
}
