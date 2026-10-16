//! Machine backends behind the worker's JSON operation protocol.
use crate::Capabilities;
use anyhow::Result;
use serde_json::Value;

/// A backend that runs machines: Firecracker on Linux, Apple Virtualization on macOS.
///
/// Operations are the worker protocol's JSON requests (`{"op":"create","id":…}`).
/// Streams (terminals, SSH) stay on each provider's own socket bridge.
pub trait ComputeProvider {
    /// Backend name, e.g. `firecracker`.
    fn backend(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    /// Runs one operation without a capability check.
    fn call(&mut self, request: &Value) -> Result<Value>;
    /// Checks the capability the operation needs, then runs it.
    fn operation(&mut self, request: &Value) -> Result<Value> {
        self.capabilities().check(self.backend(), request)?;
        self.call(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Capability;
    use serde_json::json;

    struct Fake(Vec<Value>);
    impl ComputeProvider for Fake {
        fn backend(&self) -> &'static str {
            "fake"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::of(&[Capability::Exec])
        }
        fn call(&mut self, request: &Value) -> Result<Value> {
            self.0.push(request.clone());
            Ok(json!({}))
        }
    }

    #[test]
    fn unsupported_operations_never_reach_the_backend() {
        let mut provider = Fake(Vec::new());
        assert!(provider.operation(&json!({"op":"exec"})).is_ok());
        assert!(provider.operation(&json!({"op":"hibernate"})).is_err());
        assert_eq!(provider.0, [json!({"op":"exec"})]);
    }
}
