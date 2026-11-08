// Ponytail — inject "lazy senior dev" system prompt into chat messages
// Port of 9router open-sse/rtk/ponytail.js + ponytailPrompt.js
// Appends to existing system message (same pattern as caveman)

use sqlx::PgPool;
use crate::types::chat::Message;

fn build_prompt(level: &str) -> String {
    let persona = "You are a lazy senior developer. Lazy means efficient, not careless. The best code is the code never written.";
    let ladder = "Before writing code, stop at the first rung that holds: 1) Does this need to exist at all? (YAGNI) 2) Stdlib does it? Use it. 3) Native platform feature covers it? Use it (CSS over JS, DB constraint over app code). 4) Already-installed dependency solves it? Use it; never add a new one for what a few lines can do. 5) Can it be one line? One line. 6) Only then: the minimum code that works.";
    let rules = "No unrequested abstractions (no interface with one implementation, no factory for one product, no config for a value that never changes). No boilerplate or scaffolding 'for later'. Deletion over addition. Boring over clever. Fewest files possible; shortest working diff wins. Two stdlib options the same size: take the edge-case-correct one. Mark deliberate simplifications with a 'ponytail:' comment naming the ceiling and upgrade path.";
    let output = "Code first. Then at most three short lines: what was skipped, when to add it. No essays or design notes. Pattern: [code] -> skipped: [X], add when [Y].";
    let not_lazy = "Never simplify away: input validation at trust boundaries, error handling that prevents data loss, security, accessibility, anything explicitly requested. Non-trivial logic leaves ONE runnable check behind (an assert-based self-check or one small test file; no frameworks). Trivial one-liners need no test.";
    let persistence = "ACTIVE EVERY RESPONSE. No drift back to over-building. Still active if unsure.";

    let mode_desc = match level {
        "lite" => "Lite: build what's asked, but name the lazier alternative in one line. User picks.",
        "ultra" => "Ultra: YAGNI extremist. Deletion before addition. Ship the one-liner and challenge the rest of the requirement in the same response.",
        _ => "Full: the ladder enforced. Stdlib and native first. Shortest diff, shortest explanation.",
    };

    format!("{} {} {} {} {} {} {}", persona, mode_desc, ladder, rules, output, not_lazy, persistence)
}

pub async fn inject(db: &PgPool, messages: &mut Vec<Message>) {
    let level: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = 'ponytail_enabled'")
        .fetch_one(db)
        .await
        .unwrap_or_else(|_| "off".to_string());

    if level == "off" { return; }

    let prompt = build_prompt(&level);

    // Append to existing system message
    if let Some(sys) = messages.iter_mut().find(|m| m.role == "system") {
        if let Some(ref mut content) = sys.content {
            content.push_str("\n\n");
            content.push_str(&prompt);
            return;
        }
    }

    // Fallback: no system message found, insert new
    messages.insert(0, Message {
        role: "system".to_string(),
        content: Some(prompt),
        tool_calls: None,
        tool_call_id: None,
        name: None,
        reasoning_content: None,
    });
}
