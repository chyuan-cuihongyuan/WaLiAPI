use crate::protocol::legacy::{
    anthropic_to_openai, estimate_anthropic_input_tokens, openai_to_anthropic,
};

#[test]
fn counts_structured_anthropic_input() {
    let body = serde_json::json!({
        "model": "test-model",
        "system": [{"type": "text", "text": "system prompt"}],
        "tools": [{"name": "read", "input_schema": {"type": "object"}}],
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}]
    });
    assert!(estimate_anthropic_input_tokens(&body) > 1);
}

#[test]
fn maps_tools_parallel_control_and_mixed_tool_results() {
    let request = serde_json::json!({
        "model": "claude-compatible",
        "max_tokens": 32,
        "system": [{"type":"text", "text":"be concise"}],
        "tools": [{"name":"weather", "description":"weather", "input_schema":{"type":"object"}}],
        "tool_choice": {"type":"any", "disable_parallel_tool_use":true},
        "messages": [
            {"role":"assistant", "content":[{"type":"text","text":"checking"},{"type":"tool_use","id":"call_1","name":"weather","input":{"city":"Paris"}}]},
            {"role":"user", "content":[{"type":"tool_result","tool_use_id":"call_1","content":"sunny"},{"type":"text","text":"thanks"}]}
        ]
    });
    let converted = anthropic_to_openai(&request).unwrap();
    assert_eq!(converted["parallel_tool_calls"], false);
    assert_eq!(converted["tool_choice"], "required");
    assert_eq!(
        converted["tools"][0]["function"]["parameters"]["type"],
        "object"
    );
    assert_eq!(
        converted["messages"][1]["tool_calls"][0]["function"]["arguments"],
        "{\"city\":\"Paris\"}"
    );
    assert_eq!(converted["messages"][2]["role"], "tool");
    assert_eq!(converted["messages"][3]["content"][0]["text"], "thanks");
}

#[test]
fn maps_mid_conversation_system_messages_to_chat_system_role() {
    let request = serde_json::json!({
        "model": "claude-compatible",
        "messages": [
            {"role":"user", "content":"use the strict profile"},
            {"role":"system", "content":[{"type":"text", "text":"strict profile active", "cache_control":{"type":"ephemeral"}}]},
            {"role":"assistant", "content":[{"type":"text", "text":"ack"}]}
        ]
    });
    let converted = anthropic_to_openai(&request).unwrap();
    assert_eq!(converted["messages"][0]["role"], "user");
    assert_eq!(converted["messages"][1]["role"], "system");
    assert_eq!(converted["messages"][1]["content"], "strict profile active");
    assert_eq!(converted["messages"][2]["role"], "assistant");
}

#[test]
fn legacy_tool_choice_strings_map_or_reject() {
    for (input, expected) in [("auto", "auto"), ("any", "required")] {
        let request = serde_json::json!({
            "model": "model",
            "messages": [{"role":"user", "content":"hi"}],
            "tool_choice": input
        });
        let converted = anthropic_to_openai(&request).unwrap();
        assert_eq!(converted["tool_choice"], expected);
    }

    let request = serde_json::json!({
        "model": "model",
        "messages": [{"role":"user", "content":"hi"}],
        "tool_choice": "tool"
    });
    assert!(anthropic_to_openai(&request).is_err());

    let request = serde_json::json!({
        "model": "model",
        "messages": [{"role":"user", "content":"hi"}],
        "tool_choice": "bogus"
    });
    assert!(anthropic_to_openai(&request).is_err());
}

#[test]
fn legacy_tool_use_requires_input_not_fabricated() {
    let request = serde_json::json!({
        "model": "model",
        "messages": [{"role":"assistant", "content":[{"type":"tool_use", "id":"call_1", "name":"run"}]}]
    });
    assert!(anthropic_to_openai(&request).is_err());

    let request = serde_json::json!({
        "model": "model",
        "messages": [{"role":"assistant", "content":[{"type":"tool_use", "id":"call_1", "name":"run", "input":[]}]}]
    });
    assert!(anthropic_to_openai(&request).is_err());

    let request = serde_json::json!({
        "model": "model",
        "messages": [{"role":"assistant", "content":[{"type":"tool_use", "id":"call_1", "name":"run", "input":{}}]}]
    });
    let converted = anthropic_to_openai(&request).unwrap();
    assert_eq!(
        converted["messages"][0]["tool_calls"][0]["function"]["arguments"],
        "{}"
    );
}

#[test]
fn rejects_invalid_openai_tool_arguments_without_inventing_input() {
    let response = serde_json::json!({"choices":[{"finish_reason":"tool_calls", "message":{"role":"assistant", "content":null, "tool_calls":[{"id":"call_1", "function":{"name":"run", "arguments":"{bad"}}]}}]});
    assert!(openai_to_anthropic(&response, "model").is_err());
}

#[test]
fn rejects_non_object_openai_tool_arguments_and_strips_cache_controls() {
    let response = serde_json::json!({"choices":[{"message":{"role":"assistant", "tool_calls":[{"id":"call_1", "function":{"name":"run", "arguments":"[]"}}]}}]});
    assert!(openai_to_anthropic(&response, "model").is_err());

    let cache_in_system = serde_json::json!({"model":"model", "system":[{"type":"text", "text":"cached", "cache_control":{"type":"ephemeral"}}], "messages":[]});
    assert_eq!(
        anthropic_to_openai(&cache_in_system).unwrap()["messages"][0]["content"],
        "cached"
    );
    let cache_in_message = serde_json::json!({"model":"model", "messages":[{"role":"user", "content":[{"type":"text", "text":"cached", "cache_control":{"type":"ephemeral"}}]}]});
    assert_eq!(
        anthropic_to_openai(&cache_in_message).unwrap()["messages"][0]["content"][0]["text"],
        "cached"
    );
}

#[test]
fn preserves_anthropic_response_shape_for_refusals_and_implicit_tools() {
    let refusal = serde_json::json!({"choices":[{"finish_reason":"content_filter", "message":{"role":"assistant", "content":null, "refusal":"no"}}]});
    let converted = openai_to_anthropic(&refusal, "model").unwrap();
    assert_eq!(converted["stop_reason"], "refusal");
    assert!(converted.get("stop_sequence").is_some());

    let implicit_tool = serde_json::json!({"choices":[{"finish_reason":null, "message":{"role":"assistant", "content":null, "tool_calls":[{"id":"call_1", "function":{"name":"run", "arguments":"{}"}}]}}]});
    assert_eq!(
        openai_to_anthropic(&implicit_tool, "model").unwrap()["stop_reason"],
        "tool_use"
    );
}

#[test]
fn streaming_openai_requests_always_request_late_usage() {
    let request = serde_json::json!({"model":"model", "stream":true, "stream_options":{"include_usage":false, "custom":true}, "messages":[]});
    let converted = anthropic_to_openai(&request).unwrap();
    assert_eq!(converted["stream_options"]["include_usage"], true);
    assert_eq!(converted["stream_options"]["custom"], true);
}

#[test]
fn anthropic_to_openai_maps_thinking_fail_open() {
    // thinking enabled + budget_tokens 1024 -> reasoning_effort "low".
    let body = serde_json::json!({
        "model": "m",
        "messages": [{"role": "user", "content": "u"}],
        "thinking": {"type": "enabled", "budget_tokens": 1024}
    });
    let converted = anthropic_to_openai(&body).unwrap();
    assert_eq!(converted["reasoning_effort"], "low");

    // adaptive + output_config.effort passthrough (lowercased).
    let body = serde_json::json!({
        "model": "m",
        "messages": [{"role": "user", "content": "u"}],
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": "HIGH"}
    });
    let converted = anthropic_to_openai(&body).unwrap();
    assert_eq!(converted["reasoning_effort"], "high");

    // container / context_management dropped fail-open (no error).
    let body = serde_json::json!({
        "model": "m",
        "messages": [{"role": "user", "content": "u"}],
        "container": {"type": "super_container"},
        "context_management": {"turns": 4}
    });
    let converted = anthropic_to_openai(&body).unwrap();
    assert!(converted.get("container").is_none());
    assert!(converted.get("context_management").is_none());

    // system thinking block dropped.
    let body = serde_json::json!({
        "model": "m",
        "system": [{"type": "thinking", "thinking": "instruct"}],
        "messages": []
    });
    let converted = anthropic_to_openai(&body).unwrap();
    assert_eq!(converted["messages"][0]["content"], "");

    // assistant thinking block -> reasoning_content; redacted dropped.
    let body = serde_json::json!({
        "model": "m",
        "messages": [{"role": "assistant", "content": [
            {"type": "thinking", "thinking": "chain"},
            {"type": "redacted_thinking", "data": "sig"},
            {"type": "text", "text": "answer"}
        ]}]
    });
    let converted = anthropic_to_openai(&body).unwrap();
    assert_eq!(converted["messages"][0]["reasoning_content"], "chain");
    assert_eq!(converted["messages"][0]["content"], "answer");
}

#[test]
fn openai_to_anthropic_maps_reasoning_fail_open() {
    // reasoning_content -> Messages thinking block, kept even with content.
    let response = serde_json::json!({"choices":[{"finish_reason":"stop", "message":{"role":"assistant", "reasoning_content":"chain", "content":"answer"}}]});
    let converted = openai_to_anthropic(&response, "model").unwrap();
    assert_eq!(converted["content"][0]["type"], "thinking");
    assert_eq!(converted["content"][0]["thinking"], "chain");
    assert_eq!(converted["content"][1]["type"], "text");
    assert_eq!(converted["content"][1]["text"], "answer");
}

// ---------------------------------------------------------------------------
// Upstream issue #21: Claude Code sends Anthropic built-in tools (web_search
// etc.) alongside custom function tools. Built-ins are skipped fail-open so a
// mixed request can still use an OpenAI Chat conversion channel; forcing a
// skipped built-in via tool_choice still requires a native channel.
// ---------------------------------------------------------------------------

#[test]
fn legacy_builtin_tool_alongside_custom_tool_is_skipped() {
    let request = serde_json::json!({
        "model": "claude-sonnet-4-20250514",
        "max_tokens": 1024,
        "messages": [{"role":"user", "content":"search the web for rust axum"}],
        "tools": [
            {"name": "read_file", "description": "read a file", "input_schema": {"type":"object","properties":{"path":{"type":"string"}}}},
            {"type": "web_search_20250305", "name": "web_search", "max_uses": 5}
        ],
        "tool_choice": {"type": "auto"}
    });
    let converted = anthropic_to_openai(&request).unwrap();
    let tools = converted["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1, "custom tool kept, built-in skipped");
    assert_eq!(tools[0]["function"]["name"], "read_file");
    assert_eq!(converted["tool_choice"], "auto");
}

#[test]
fn legacy_builtin_tool_only_drops_tools_and_tool_choice() {
    let request = serde_json::json!({
        "model": "claude-sonnet-4-20250514",
        "max_tokens": 1024,
        "messages": [{"role":"user", "content":"hi"}],
        "tools": [
            {"type": "web_search_20250305", "name": "web_search"}
        ],
        "tool_choice": {"type": "auto"}
    });
    let converted = anthropic_to_openai(&request).unwrap();
    assert!(converted.get("tools").is_none(), "no tools survive");
    // OpenAI rejects tool_choice without tools, so it must be dropped too.
    assert!(converted.get("tool_choice").is_none());
}

#[test]
fn legacy_tool_choice_forcing_builtin_tool_still_rejected() {
    let request = serde_json::json!({
        "model": "claude-sonnet-4-20250514",
        "max_tokens": 1024,
        "messages": [{"role":"user", "content":"hi"}],
        "tools": [
            {"name": "read_file", "description": "read a file", "input_schema": {"type":"object","properties":{}}},
            {"type": "web_search_20250305", "name": "web_search"}
        ],
        "tool_choice": {"type": "tool", "name": "web_search"}
    });
    assert!(anthropic_to_openai(&request).is_err());
}

#[test]
fn legacy_tool_choice_any_with_only_builtin_tools_rejected() {
    let request = serde_json::json!({
        "model": "claude-sonnet-4-20250514",
        "max_tokens": 1024,
        "messages": [{"role":"user", "content":"hi"}],
        "tools": [
            {"type": "web_search_20250305", "name": "web_search"}
        ],
        "tool_choice": {"type": "any"}
    });
    assert!(anthropic_to_openai(&request).is_err());
}

#[test]
fn legacy_unknown_tool_type_still_rejected() {
    let request = serde_json::json!({
        "model": "claude-sonnet-4-20250514",
        "max_tokens": 1024,
        "messages": [{"role":"user", "content":"hi"}],
        "tools": [
            {"type": "mystery_tool_20990101", "name": "mystery"}
        ]
    });
    assert!(anthropic_to_openai(&request).is_err());
}

#[test]
fn legacy_builtin_tool_families_recognized() {
    for tool_type in [
        "web_search_20250305",
        "computer_20250124",
        "text_editor_20250429",
        "code_execution_20250522",
        "bash_20250124",
    ] {
        let request = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role":"user", "content":"hi"}],
            "tools": [{"type": tool_type, "name": "t"}]
        });
        let converted = anthropic_to_openai(&request)
            .unwrap_or_else(|e| panic!("{tool_type} should be skipped, got: {e}"));
        assert!(converted.get("tools").is_none(), "{tool_type}");
    }
}
