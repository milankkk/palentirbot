use std::sync::Arc;

use command_macros::SlashCommand;
use eyre::Result;
use twilight_interactions::command::{CommandModel, CreateCommand};
use twilight_model::{
    channel::{
        message::{
            embed::{Embed, EmbedField},
            Message,
        },
    },
    id::{
        marker::{ChannelMarker, GuildMarker},
        Id,
    },
};

use crate::{
    core::Context,
    util::{
        builder::{EmbedBuilder, MessageBuilder},
        interaction::InteractionCommand,
        InteractionCommandExt,
    },
};

#[derive(CommandModel, CreateCommand, SlashCommand)]
#[command(name = "aicontext")]
#[flags(SKIP_DEFER)]
/// Show AI context window usage, memory breakdown, and engine stats
pub struct AiContext;

async fn slash_aicontext(ctx: Arc<Context>, command: InteractionCommand) -> Result<()> {
    if !crate::ai::is_enabled_for_guild(&ctx, command.guild_id) {
        command
            .error_callback(&ctx, crate::ai::AI_DISABLED_REPLY, true)
            .await?;
        return Ok(());
    }

    let embed = create_context_embed(&ctx, command.channel_id, command.guild_id).await;
    let builder = MessageBuilder::new().embed(embed);
    command.callback(&ctx, builder, true).await?;

    Ok(())
}

/// Build the rich context breakdown embed for both slash and prefix commands.
pub async fn create_context_embed(
    ctx: &Context,
    channel_id: Id<ChannelMarker>,
    guild_id: Option<Id<GuildMarker>>,
) -> Embed {
    let ai = match ctx.ai() {
        Some(ai) => ai,
        None => {
            return EmbedBuilder::new()
                .title("🤖 AI Context Window Breakdown")
                .description("The AI module is currently disabled.")
                .color(0x00A8FC)
                .build();
        }
    };

    let bot_id = ctx
        .cache
        .current_user(|user| user.id)
        .expect("missing CurrentUser in cache");

    // 1. System prompt metrics
    let prompt = ai.get_prompt().await;
    let prompt_chars = prompt.chars().count();
    let prompt_bytes = prompt.len();
    // In English & slang text with Qwen tokenizer, 1 token is ~3.8 characters
    let prompt_tokens = (prompt_chars as f64 / 3.8).round() as usize;

    // 2. Channel conversation history metrics
    let history = fetch_channel_history(ctx, channel_id, guild_id).await.unwrap_or_default();
    let total_messages = history.len();
    let mut user_messages = 0;
    let mut bot_messages = 0;
    let mut history_chars = 0;

    for msg in &history {
        if msg.author.id == bot_id {
            bot_messages += 1;
        } else {
            user_messages += 1;
        }
        history_chars += msg.content.chars().count();
    }

    let history_tokens = (history_chars as f64 / 3.8).round() as usize;

    // 3. Overall window metrics
    let total_used_tokens = prompt_tokens + history_tokens;
    let max_window = (ai.num_ctx() as usize).max(1);
    let free_tokens = max_window.saturating_sub(total_used_tokens);
    let used_percent = ((total_used_tokens as f64 / max_window as f64) * 100.0).clamp(0.0, 100.0);
    let free_percent = (100.0 - used_percent).max(0.0);

    let progress_bar = make_progress_bar(total_used_tokens, max_window, 16);

    // 4. Last actual generation stats from Ollama (if available)
    let last_stats = ai.last_stats(channel_id).await;

    // 5. Hardware & queue info
    let queue_len = ai.queue_len().await;
    let queue_status = if queue_len == 0 {
        "Idle (0 waiting)".to_string()
    } else {
        format!("{queue_len} active/queued")
    };

    let model_name = ai.model_name();

    let mut fields = Vec::with_capacity(5);

    fields.push(EmbedField {
        inline: true,
        name: "📋 System Prompt (`prompt.txt`)".to_string(),
        value: format!(
            "**~{}** tokens\n`{:.1} KB` • `{} chars`",
            format_num(prompt_tokens),
            prompt_bytes as f64 / 1024.0,
            format_num(prompt_chars)
        ),
    });

    fields.push(EmbedField {
        inline: true,
        name: "💬 Chat History".to_string(),
        value: format!(
            "**~{}** tokens\n`{total_messages}` msgs ({user_messages} user, {bot_messages} bot)",
            format_num(history_tokens),
        ),
    });

    fields.push(EmbedField {
        inline: true,
        name: "🆓 Free Headroom".to_string(),
        value: format!(
            "**~{}** tokens\n`{free_percent:.1}%` available",
            format_num(free_tokens),
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
                "• **Evaluated:** `{}` prompt tokens\n\
                 • **Reasoning:** `{}` thinking tokens\n\
                 • **Generated:** `{}` tokens in `{:.2}s` (`{speed}`)\n\
                 • **Load Duration:** `{:.2}s`",
                format_num(stats.prompt_tokens),
                format_num(stats.thinking_tokens),
                format_num(stats.response_tokens),
                stats.eval_duration_ms as f64 / 1000.0,
                stats.load_duration_ms as f64 / 1000.0,
            ),
        });
    }

    let num_predict = ai.num_predict();

    fields.push(EmbedField {
        inline: false,
        name: "🧠 Model & Memory Allocation".to_string(),
        value: format!(
            "• **Model:** `{model_name}`\n\
             • **Total Context:** `{}` tokens (max `{}` predicted)\n\
             • **Memory:** GPU VRAM (weights & active cache) + System RAM (context overflow)\n\
             • **Queue Status:** {queue_status}",
            format_num(max_window),
            format_num(num_predict as usize)
        ),
    });

    let description = format!(
        "**Context Window Usage:**\n\
         `{progress_bar}` • **{} / {}** tokens",
        format_num(total_used_tokens),
        format_num(max_window)
    );

    EmbedBuilder::new()
        .title("🤖 AI Context Window Breakdown")
        .color(0x00A8FC)
        .description(description)
        .fields(fields)
        .build()
}

fn format_num(n: usize) -> String {
    let s = n.to_string();
    let mut result = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && (chars.len() - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(*c);
    }
    result
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

async fn fetch_channel_history(
    ctx: &Context,
    channel_id: Id<ChannelMarker>,
    guild_id: Option<Id<GuildMarker>>,
) -> Result<Vec<Message>> {
    let limit = std::env::var("AI_HISTORY_MESSAGES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let response = ctx.http.channel_messages(channel_id).limit(limit).await?;
    let mut messages: Vec<Message> = response.model().await?;

    if let Some(ai) = ctx.ai() {
        if let Some(cutoff) = ai.get_cutoff(channel_id, guild_id).await {
            messages.retain(|msg| msg.timestamp.as_secs() > cutoff);
        }
    }

    Ok(messages)
}
