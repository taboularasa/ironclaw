//! System prompt construction for the execution loop.
//!
//! Builds a CodeAct/RLM system prompt that instructs the LLM to write
//! Python code in ```repl blocks with tools available as callable functions.
//!
//! Prompt templates live in `crates/ironclaw_engine/prompts/` as plain
//! markdown files for easy inspection and iteration. They are embedded
//! at compile time via `include_str!`.

use crate::types::capability::ActionDef;

/// The main instruction block (before tool listing).
const CODEACT_PREAMBLE: &str = include_str!("../../prompts/codeact_preamble.md");

/// The strategy/closing block (after tool listing).
const CODEACT_POSTAMBLE: &str = include_str!("../../prompts/codeact_postamble.md");

/// Build the system prompt for CodeAct/RLM execution.
///
/// The prompt instructs the LLM to:
/// - Write Python code in ```repl fenced blocks
/// - Call tools as regular Python functions
/// - Use llm_query(prompt, context) for sub-agent calls
/// - Use FINAL(answer) to return the final answer
/// - Access thread context via the `context` variable
pub fn build_codeact_system_prompt(actions: &[ActionDef]) -> String {
    let mut prompt = String::from(CODEACT_PREAMBLE);

    // Add tool documentation
    if !actions.is_empty() {
        prompt.push_str("\n## Available tools (call as Python functions)\n\n");
        for action in actions {
            prompt.push_str(&format!("- `{}(", action.name));
            // Extract parameter names from JSON schema
            if let Some(props) = action.parameters_schema.get("properties")
                && let Some(obj) = props.as_object()
            {
                let params: Vec<&str> = obj.keys().map(String::as_str).collect();
                prompt.push_str(&params.join(", "));
            }
            prompt.push_str(&format!(")` — {}\n", action.description));
        }
    }

    prompt.push_str(CODEACT_POSTAMBLE);
    prompt
}
