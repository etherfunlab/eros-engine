# `model_config.d` — the example config, split by subsystem

The same config as [`../model_config.toml`](../model_config.toml), cut into one
file per subsystem for directory mode
([docs/model-config.md → Directory mode](../../docs/model-config.md#directory-mode)):

```bash
MODEL_CONFIG_PATH= MODEL_CONFIG_DIR=examples/model_config.d eros-engine serve
```

| File | Holds |
|---|---|
| `providers.toml` | `[providers]` entries and body rules, plus the notes on model forms |
| `chat.toml` | `chat_companion`, and the optional `chat_output_filter`, `chat_input_filter`, `chat_voice` |
| `chat_vision.toml` | the optional `chat_vision`, `chat_image_prompt_compose`, `chat_image_edit_compose` |
| `insights.toml` | `insight_extraction`, `profile_rebuild` |
| `character_insights.toml` | `character_insight_extraction`, `character_insight_structuring` |
| `user_insights.toml` | `user_insight_extraction`, `user_insight_structuring` |
| `memory.toml` | `dreaming_scan`, `arc_rewrite` |
| `decision.toml` | `pde_decision`, and the optional `chat_product_qa` |
| `affinity.toml` | `affinity_evaluation`, `affinity_summary` |
| `group.toml` | `group_director` |
| `world.toml` | `world_director`, `world_stories_director`, `world_comment`, `world_reply` |
| `embedding.toml` | `embedding` |

Each `[tasks.<name>]` and every other top-level key lives in exactly one file;
a second definition fails boot naming both files. A test
(`example_config_dir_matches_the_single_file_example` in
`crates/eros-engine-llm/src/model_config.rs`) fails when this directory and
`../model_config.toml` stop being the same config — change both together.
