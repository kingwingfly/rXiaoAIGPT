//! The set of capabilities the model is allowed to use.
//!
//! The registry is the whole of the "adding a capability" story. There is no
//! enum of intents, no match on tool names, and no place in the control loop
//! that has to learn about a new tool: you write one [`Tool`] impl and register
//! it, and the model discovers it from [`Tool::description`] and
//! [`Tool::parameters`]. Anything that would require a *second* edit somewhere
//! else is a design bug in this module.
//!
//! ```
//! use brain::{Result, Tool, ToolRegistry};
//! use serde_json::{Value, json};
//!
//! struct Ping;
//!
//! #[brain::async_trait]
//! impl Tool for Ping {
//!     fn name(&self) -> &str { "ping" }
//!     fn description(&self) -> &str { "Check the assistant is alive." }
//!     fn parameters(&self) -> Value { json!({ "type": "object", "properties": {} }) }
//!     async fn call(&self, _args: Value) -> Result<String> { Ok("pong".into()) }
//! }
//!
//! let registry = ToolRegistry::new().with(Ping);
//! assert_eq!(registry.schemas()[0]["function"]["name"], "ping");
//! ```

use crate::error::{BrainErr, Result};
use crate::traits::Tool;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Tools the model may call, keyed by [`Tool::name`].
///
/// Backed by a `BTreeMap` rather than a `HashMap` so [`ToolRegistry::schemas`]
/// has a stable order. That is not cosmetic: tool order is prompt content, and a
/// prompt that reshuffles between runs makes the model's behaviour irreproducible
/// and defeats provider-side prompt caching.
///
/// Cloning is cheap — the tools sit behind [`Arc`] — so a registry can be shared
/// with anything that needs to dispatch.
#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// An empty registry. A model given one of these simply always answers in
    /// prose, which is a legitimate configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tool, taking ownership.
    ///
    /// Registering two tools under the same name replaces the first and logs a
    /// warning: silently keeping both would give the model an ambiguous list,
    /// and panicking would take down a process over a configuration slip.
    pub fn register(&mut self, tool: impl Tool + 'static) -> &mut Self {
        self.register_arc(Arc::new(tool))
    }

    /// Add an already-shared tool. Useful when the same instance is also held
    /// elsewhere — a tool wrapping a [`crate::Speaker`] handle, say.
    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        let name = tool.name().to_string();
        if self.tools.insert(name.clone(), tool).is_some() {
            tracing::warn!(tool = %name, "tool re-registered under an existing name; replacing");
        }
        self
    }

    /// Builder-style [`ToolRegistry::register`], for chaining at construction:
    /// `ToolRegistry::new().with(a).with(b)`.
    #[must_use]
    pub fn with(mut self, tool: impl Tool + 'static) -> Self {
        self.register(tool);
        self
    }

    /// Builder-style [`ToolRegistry::register_arc`].
    #[must_use]
    pub fn with_arc(mut self, tool: Arc<dyn Tool>) -> Self {
        self.register_arc(tool);
        self
    }

    /// Look one up by the name the model used.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.get(name)
    }

    /// Registered names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// The OpenAI `tools` array, ready to hand to
    /// [`crate::LlmClient::chat`].
    ///
    /// Built here rather than in the client so that the provider crate's types
    /// never leak into [`Tool`]: an implementor writes a plain
    /// [`serde_json::Value`] schema and nothing else.
    pub fn schemas(&self) -> Vec<serde_json::Value> {
        self.tools
            .values()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name(),
                        "description": tool.description(),
                        "parameters": tool.parameters(),
                    }
                })
            })
            .collect()
    }

    /// Run the named tool.
    ///
    /// An unknown name is a [`BrainErr::NotFound`] listing what *is* available —
    /// never a panic. Models do hallucinate tool names, and the loop's job is to
    /// hand that error back so the model can pick a real one on the next turn,
    /// which it cannot do without knowing the alternatives.
    pub async fn dispatch(&self, name: &str, args: serde_json::Value) -> Result<String> {
        let Some(tool) = self.tools.get(name) else {
            let known = self.names().collect::<Vec<_>>().join(", ");
            tracing::warn!(tool = %name, "model called an unknown tool");
            return Err(BrainErr::NotFound(format!(
                "no tool named `{name}`; available tools: [{known}]"
            )));
        };

        tracing::info!(tool = %name, args = %args, "dispatching tool call");
        tool.call(args).await
    }
}

impl std::fmt::Debug for ToolRegistry {
    /// `dyn Tool` is not `Debug`, so print the names — which is the only part
    /// anyone debugging a registry wants anyway.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.names().collect::<Vec<_>>())
            .finish()
    }
}

impl FromIterator<Arc<dyn Tool>> for ToolRegistry {
    fn from_iter<I: IntoIterator<Item = Arc<dyn Tool>>>(iter: I) -> Self {
        let mut registry = Self::new();
        for tool in iter {
            registry.register_arc(tool);
        }
        registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// Echoes its `text` argument, or fails when told to — enough to exercise
    /// both dispatch outcomes.
    struct Echo {
        name: &'static str,
    }

    #[async_trait::async_trait]
    impl Tool for Echo {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "Echo the given text back."
        }
        fn parameters(&self) -> Value {
            json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            })
        }
        async fn call(&self, args: Value) -> Result<String> {
            match args.get("text").and_then(Value::as_str) {
                Some("boom") => Err(BrainErr::Backend("exploded".into())),
                Some(text) => Ok(text.to_string()),
                None => Err(BrainErr::InvalidArguments("`text` is required".into())),
            }
        }
    }

    #[test]
    fn schemas_are_openai_shaped_and_ordered() {
        let registry = ToolRegistry::new()
            .with(Echo { name: "zebra" })
            .with(Echo { name: "alpha" });

        let schemas = registry.schemas();
        assert_eq!(schemas.len(), 2);
        // Sorted, not insertion-ordered: the prompt must be reproducible.
        assert_eq!(schemas[0]["function"]["name"], "alpha");
        assert_eq!(schemas[1]["function"]["name"], "zebra");
        assert_eq!(schemas[0]["type"], "function");
        assert_eq!(
            schemas[0]["function"]["description"],
            "Echo the given text back."
        );
        assert_eq!(schemas[0]["function"]["parameters"]["type"], "object");
    }

    #[tokio::test]
    async fn dispatch_routes_by_name() {
        let registry = ToolRegistry::new().with(Echo { name: "echo" });
        let out = registry
            .dispatch("echo", json!({"text": "hi"}))
            .await
            .unwrap();
        assert_eq!(out, "hi");
    }

    #[tokio::test]
    async fn an_unknown_tool_errors_cleanly_and_names_the_alternatives() {
        let registry = ToolRegistry::new().with(Echo { name: "echo" });
        let err = registry.dispatch("nope", json!({})).await.unwrap_err();
        assert!(matches!(err, BrainErr::NotFound(_)));
        let message = err.to_string();
        assert!(message.contains("nope"), "{message}");
        assert!(message.contains("echo"), "{message}");
    }

    #[tokio::test]
    async fn a_failing_tool_surfaces_its_error() {
        let registry = ToolRegistry::new().with(Echo { name: "echo" });
        let err = registry
            .dispatch("echo", json!({"text": "boom"}))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "exploded");
    }

    #[test]
    fn re_registering_a_name_replaces_rather_than_duplicating() {
        let registry = ToolRegistry::new()
            .with(Echo { name: "echo" })
            .with(Echo { name: "echo" });
        assert_eq!(registry.len(), 1);
        assert!(registry.get("echo").is_some());
    }

    #[test]
    fn a_registry_can_be_collected_from_shared_tools() {
        let tools: Vec<Arc<dyn Tool>> =
            vec![Arc::new(Echo { name: "a" }), Arc::new(Echo { name: "b" })];
        let registry: ToolRegistry = tools.into_iter().collect();
        assert_eq!(registry.names().collect::<Vec<_>>(), ["a", "b"]);
    }
}
