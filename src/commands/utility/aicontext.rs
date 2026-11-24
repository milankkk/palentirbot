use std::sync::Arc;

use command_macros::SlashCommand;
use eyre::Result;
use twilight_interactions::command::{CommandModel, CreateCommand};
use twilight_model::{
    channel::message::embed::{Embed, EmbedField},
    id::{marker::ChannelMarker, Id},
};

use super::ask::{estimate_tokens, HISTORY_MESSAGE_CHAR_LIMIT};
use crate::{
    core::Context,
    util::{
        builder::{EmbedBuilder, MessageBuilder},
        interaction::InteractionCommand,
        numbers::with_comma_int,
        Authored, InteractionCommandExt,
    },
};

#[derive(CommandModel, CreateCommand, SlashCommand)]
#[command(name = "aicontext")]
#[flags(SKIP_DEFER)]
/// Show AI context window usage, memory breakdown, and engine stats
pub struct AiContext;

async fn slash_aicontext(ctx: Arc<Context>, command: InteractionCommand) -> Result<()> {
    let user_id = command.user_id()?;
    if let Err(reason) = super::ask::check_ai_access(&ctx, user_id, command.guild_id).await {
        command.error_callback(&ctx, reason, true).await?;
        return Ok(());
    }

    command.defer(&ctx, true).await?;
    let embed = create_context_embed(&ctx, command.channel_id).await;
    let builder = MessageBuilder::new().embed(embed);
    command.update(&ctx, &builder).await?;

    Ok(())
}

/// Build the rich context breakdown embed for both slash and prefix commands.
pub async fn create_context_embed(ctx: &Context, channel_id: Id<ChannelMarker>) -> Embed {
    let bot_id = ctx.bot_user_id();

    // 1. System prompt metrics
    let (prompt_chars, prompt_bytes) = ctx.ai().prompt_metrics().await;
    let prompt_tokens = estimate_tokens(prompt_chars);

    let guild_id = ctx.cache.channel(channel_id, |c| c.guild_id).ok().flatten();
    let guild_prefix = ctx.guild_prefix(guild_id);

    // 2. Channel conversation history metrics (filtered using the same rules as ask.rs)
    let history = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        super::ask::get_history(ctx, channel_id, None),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or_default();
    let slash_turns = ctx
        .ai()
        .slash_turns_for(history.iter().map(|m| m.id))
        .await;
    let mut user_messages = 0;
    let mut bot_messages = 0;
    let mut history_chars = 0;

    for msg in &history {
        let formatted = super::ask::format_message_text_and_embeds(msg);
        let trimmed = formatted.trim();
        if trimmed.is_empty() {
            continue;
        }
        if msg.author.bot && msg.author.id != bot_id {
            continue;
        }
        if msg.author.id == bot_id {
            if super::ask::is_transient_status(trimmed) {
                continue;
            }
            bot_messages += 1;
            if let Some(slash_prompt) = slash_turns.get(&msg.id) {
                user_messages += 1;
                history_chars += slash_prompt.chars().count().min(HISTORY_MESSAGE_CHAR_LIMIT);
            }
        } else {
            if super::ask::is_non_ai_command_with_prefix(trimmed, Some(guild_prefix.as_ref())) {
                continue;
            }
            user_messages += 1;
        }
        history_chars += trimmed.chars().count().min(HISTORY_MESSAGE_CHAR_LIMIT);
    }
    let total_messages = user_messages + bot_messages;

    let history_tokens = estimate_tokens(history_chars);

    // 3. Overall window metrics
    let cfg = ctx.ai().runtime_config();
    let total_used_tokens = prompt_tokens + history_tokens;
    let max_window = (cfg.num_ctx as usize).max(1);
    let free_tokens = max_window.saturating_sub(total_used_tokens);
    let used_percent = ((total_used_tokens as f64 / max_window as f64) * 100.0).clamp(0.0, 100.0);
    let free_percent = (100.0 - used_percent).max(0.0);

    let progress_bar = make_progress_bar(total_used_tokens, max_window, 16);

    // 4. Last actual generation stats from Ollama (if available)
    let last_stats = ctx.ai().last_stats(channel_id).await;

    // 5. Hardware & queue info
    let queue_len = ctx.ai().queue_len().await;
    let queue_status = if queue_len == 0 {
        "Idle (0 waiting)".to_string()
    } else {
        format!("{queue_len} active/queued")
    };

    let mut fields = Vec::with_capacity(5);

    fields.push(EmbedField {
        inline: true,
        name: "📋 System Prompt (`prompt.txt`)".to_string(),
        value: format!(
            "**~{}** tokens\n`{:.1} KB` • `{} chars`",
            with_comma_int(prompt_tokens),
            prompt_bytes as f64 / 1024.0,
            with_comma_int(prompt_chars)
        ),
    });

    fields.push(EmbedField {
        inline: true,
        name: "💬 Chat History".to_string(),
        value: format!(
            "**~{}** tokens\n`{total_messages}` msgs ({user_messages} user, {bot_messages} bot)",
            with_comma_int(history_tokens),
        ),
    });

    fields.push(EmbedField {
        inline: true,
        name: "🆓 Free Headroom".to_string(),
        value: format!(
            "**~{}** tokens\n`{free_percent:.1}%` available",
            with_comma_int(free_tokens),
        ),
    });

    if let Some(stats) = last_stats {
        let speed = if stats.eval_duration_ms > 0 {
            format!(
                "{:.1} tok/s",
                (stats.response_tokens as f64 / (stats.eval_duration_ms as f64 / 1000.0))
            )
        } else {
            "N/A".to_string()
        };

        fields.push(EmbedField {
            inline: false,
            name: "⚡ Last Generation (Engine Verified)".to_string(),
            value: format!(
                "**Evaluated:** `{}` prompt tokens • **Generated:** `{}` tokens in `{:.2}s` (`{speed}`)",
                with_comma_int(stats.prompt_tokens),
                with_comma_int(stats.response_tokens),
                stats.eval_duration_ms as f64 / 1000.0,
            ),
        });
    }

    let num_predict_display = if cfg.num_predict < 0 {
        "unlimited".to_owned()
    } else {
        with_comma_int(cfg.num_predict as usize).to_string()
    };

    fields.push(EmbedField {
        inline: false,
        name: "🧠 Model & Memory Allocation".to_string(),
        value: format!(
            "• **Model:** `{}`\n\
             • **Total Context:** `{}` tokens (max `{}` predicted)\n\
             • **Memory:** Managed by Ollama runtime\n\
             • **Queue Status:** {queue_status}",
            cfg.model,
            with_comma_int(max_window),
            num_predict_display
        ),
    });

    let description = format!(
        "**Context Window Usage:**\n\
         `{progress_bar}` • **{} / {}** tokens",
        with_comma_int(total_used_tokens),
        with_comma_int(max_window)
    );

    EmbedBuilder::new()
        .title("🤖 AI Context Window Breakdown")
        .color(0x00A8FC)
        .description(description)
        .fields(fields)
        .build()
}

fn make_progress_bar(used: usize, total: usize, width: usize) -> String {
    let ratio = if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64).clamp(0.0, 1.0)
    };
    let filled = (ratio * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    let percent = ratio * 100.0;
    format!("[{}{}] {:.1}%", "█".repeat(filled), "░".repeat(empty), percent)
}
