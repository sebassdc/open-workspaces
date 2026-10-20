//! Controller-only operations, deliberately absent from runner `Objects`.
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListedObject {
    pub key: String,
    pub bytes: u64,
    pub modified: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObjectVersion {
    pub bytes: u64,
    pub version: String,
}
/// Implementations must distinguish absence from authorization/transport failures.
pub trait ControllerObjects: Objects {
    fn configuration(&self) -> Result<()>;
    /// Unique write-once run marker; timestamp comes from the store's HEAD response.
    fn marker(&self, run: &str) -> Result<i64>;
    fn list(&self) -> Result<Vec<ListedObject>>;
    fn head(&self, key: &str) -> Result<Option<ObjectVersion>>;
    /// Writes a delete marker, never deletes a version. Returns its version ID.
    fn delete(&self, key: &str) -> Result<String>;
}
