use eyre::{Context, Report, Result};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use crate::{
    core::{
        ai::search::validate_and_resolve_url, replay_queue::process::extract_zip_safely,
        BotConfig, Context as TwilightContext,
    },
    custom_client::MAX_SKIN_BYTES,
    util::{
        builder::MessageBuilder,
        constants::GENERAL_ISSUE,
        interaction::InteractionCommand,
        str::percent_decode_utf8,
        InteractionCommandExt,
    },
};

use super::SkinAdd;

static FLATTEN_SEQ: AtomicU64 = AtomicU64::new(1);

pub(crate) fn sanitize_skin_dir_name(raw: &str) -> String {
    let decoded = percent_decode_utf8(raw.trim());
    let lower = decoded.to_ascii_lowercase();
    let stripped = if lower.ends_with(".osk") || lower.ends_with(".zip") {
        &decoded[..decoded.len() - 4]
    } else {
        decoded.as_str()
    };

    let mut cleaned: String = stripped
        .chars()
        .map(|c| {
            if c.is_control()
                || matches!(
                    c,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'
                )
            {
                '_'
            } else {
                c
            }
        })
        .collect();

    while cleaned.contains("..") {
        cleaned = cleaned.replace("..", "_");
    }

    let trimmed = cleaned.trim_matches(|c: char| c.is_whitespace() || c == '.' || c == '_');
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return "skin".to_owned();
    }

    let clamped: String = trimmed.chars().take(80).collect();
    let final_trimmed = clamped.trim_matches(|c: char| c.is_whitespace() || c == '.' || c == '_');
    if final_trimmed.is_empty() || final_trimmed == "." || final_trimmed == ".." {
        "skin".to_owned()
    } else {
        final_trimmed.to_owned()
    }
}

pub async fn add(
    ctx: Arc<TwilightContext>,
    command: InteractionCommand,
    args: SkinAdd,
) -> Result<()> {
    if !super::ensure_owner(&ctx, &command).await? {
        return Ok(());
    }

    let SkinAdd { skin, url } = args;

    let (bytes, filename) = match (skin, url) {
        (Some(skin), None) => {
            if !matches!(skin.filename.rsplit('.').next(), Some(ext) if ext.eq_ignore_ascii_case("osk")) {
                let content = "The attachment must be a .osk file!";
                command.error_callback(&ctx, content, true).await?;
                return Ok(());
            }
            if skin.size == 0 || skin.size > MAX_SKIN_BYTES as u64 {
                let content = format!(
                    "The `.osk` attachment must be between 1 byte and {} MB.",
                    MAX_SKIN_BYTES / (1024 * 1024)
                );
                command.error_callback(&ctx, content, true).await?;
                return Ok(());
            }
            let filename = sanitize_skin_dir_name(&skin.filename);
            let builder = MessageBuilder::new().embed("Downloading...");
            command.callback(&ctx, builder, false).await?;
            let bytes = match ctx
                .client()
                .get_discord_attachment_limited(&skin, MAX_SKIN_BYTES)
                .await
            {
                Ok(bytes) => bytes,
                Err(err) => {
                    let _ = command.error(&ctx, "Failed to download attachment").await;
                    return Err(err.wrap_err("failed to download skin attachment"));
                }
            };
            (bytes, filename)
        }
        (None, Some(url)) => {
            command.defer(&ctx, false).await?;

            let target = match validate_and_resolve_url(&url).await {
                Ok(t) => t,
                Err(err) => {
                    command
                        .error(&ctx, format!("Invalid or disallowed URL: {err}"))
                        .await?;
                    return Ok(());
                }
            };

            if !target.url.path().to_ascii_lowercase().ends_with(".osk") {
                command
                    .error(&ctx, "The URL must point to a .osk file!")
                    .await?;
                return Ok(());
            }

            let builder = MessageBuilder::new().embed("Downloading from URL...");
            command.update(&ctx, &builder).await?;

            let bytes = match ctx.client().get_skin_from_url(&url).await {
                Ok(b) => b,
                Err(err) => {
                    let _ = command.error(&ctx, "Failed to download from URL").await;
                    return Err(err.wrap_err("failed to download skin from url"));
                }
            };

            let raw_filename = target
                .url
                .path_segments()
                .and_then(|mut segs| segs.next_back())
                .unwrap_or("skin");
            let filename = sanitize_skin_dir_name(raw_filename);

            (bytes, filename)
        }
        (Some(_), Some(_)) => {
            command
                .error_callback(&ctx, "Provide either a file or a URL, not both!", true)
                .await?;
            return Ok(());
        }
        (None, None) => {
            command
                .error_callback(&ctx, "Provide either a .osk file or a direct URL!", true)
                .await?;
            return Ok(());
        }
    };

    let mut builder = MessageBuilder::new().embed("Generating skin name...");
    command.update(&ctx, &builder).await?;

    let skins_root = BotConfig::get().paths.skins();
    let mut skin_file = skins_root.clone();

    let filename_bytes = filename.as_bytes();
    let search_res = {
        let mut skin_list = ctx.skin_list();
        let skins = skin_list.get()?;
        skins.binary_search_by(|skin| {
            skin.as_encoded_bytes()
                .iter()
                .map(u8::to_ascii_lowercase)
                .cmp(filename_bytes.iter().map(u8::to_ascii_lowercase))
        })
    };

    let idx = match search_res {
        Ok(idx) => {
            let mut suffix = 1;

            loop {
                skin_file.push(format!("{filename}_{suffix}"));

                if !tokio::fs::try_exists(&skin_file).await.unwrap_or(false) {
                    break idx + suffix + 1;
                }

                skin_file.pop();
                suffix += 1;
            }
        }
        Err(idx) => {
            skin_file.push(&filename);

            idx + 1
        }
    };

    if skin_file.parent() != Some(skins_root.as_path()) {
        let _ = command.error(&ctx, "Invalid target skin directory.").await;
        eyre::bail!("resolved skin path {:?} escapes {:?}", skin_file, skins_root);
    }

    builder = builder.embed("Extracting...");
    command.update(&ctx, &builder).await?;

    const MAX_UNCOMPRESSED_SKIN_BYTES: u64 = 512 * 1024 * 1024; // 512 MB
    const MAX_SKIN_ENTRIES: usize = 5_000;
    let staging_seq = FLATTEN_SEQ.fetch_add(1, Ordering::Relaxed);
    let staging_dir = skins_root.join(format!(".tmp_skin_{staging_seq}"));
    let staging_dir_clone = staging_dir.clone();
    let skin_file_clone = skin_file.clone();
    let skins_root_clone = skins_root.clone();
    let extract_res = tokio::task::spawn_blocking(move || -> Result<bool, Report> {
        extract_zip_safely(
            &bytes,
            &staging_dir_clone,
            MAX_UNCOMPRESSED_SKIN_BYTES,
            MAX_SKIN_ENTRIES,
        )?;

        let mut skin_ini_path = staging_dir_clone.clone();
        skin_ini_path.push("skin.ini");

        if !(case_insensitive_exists(&skin_ini_path)?
            || move_directory(&skins_root_clone, &staging_dir_clone)?)
        {
            if staging_dir_clone.parent() == Some(skins_root_clone.as_path()) {
                fs::remove_dir_all(&staging_dir_clone).with_context(|| {
                    format!(
                        "failed to remove staging directory after failing to assure skin.ini for {staging_dir_clone:?}"
                    )
                })?;
            }
            return Ok(false);
        }

        if skin_file_clone.exists() {
            let _ = fs::remove_dir_all(&staging_dir_clone);
            eyre::bail!("target skin directory `{skin_file_clone:?}` already exists");
        }

        fs::rename(&staging_dir_clone, &skin_file_clone).with_context(|| {
            format!(
                "failed moving validated skin from {staging_dir_clone:?} to {skin_file_clone:?}"
            )
        })?;

        Ok(true)
    })
    .await;

    if !matches!(extract_res, Ok(Ok(true))) && staging_dir.parent() == Some(skins_root.as_path()) {
        let _ = tokio::fs::remove_dir_all(&staging_dir).await;
    }

    match extract_res {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {
            let content = "There was an error getting the folder containing the skin elements! \
                Try re-exporting the skin!";
            command.error(&ctx, content).await?;
            return Ok(());
        }
        Ok(Err(err)) => {
            let _ = command.error(&ctx, err.to_string()).await;
            return Err(err);
        }
        Err(err) => {
            let _ = command.error(&ctx, GENERAL_ISSUE).await;
            return Err(Report::from(err).wrap_err("extraction task panicked"));
        }
    }

    // Reset the skin list cache
    ctx.skin_list().clear();

    let content = format!("Added skin to list at index `{idx}`");
    builder = builder.embed(content);
    command.update(&ctx, &builder).await?;

    Ok(())
}

fn move_directory(skins_root: &Path, to: &Path) -> Result<bool> {
    let entries = fs::read_dir(to).with_context(|| format!("failed to read directory at {to:?}"))?;

    let mut inner_dir: Option<PathBuf> = None;
    for res in entries {
        let entry = res.with_context(|| format!("failed to read entry of directory at {to:?}"))?;
        let name = entry.file_name();
        let lossy = name.to_string_lossy();
        if lossy == "__MACOSX" || lossy.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() && case_insensitive_exists(path.join("skin.ini"))? {
            inner_dir = Some(path);
            break;
        }
    }

    let Some(from) = inner_dir else {
        return Ok(false);
    };

    let seq = FLATTEN_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_dir = skins_root.join(format!(".flatten_tmp_{seq}"));
    fs::rename(&from, &tmp_dir)
        .with_context(|| format!("failed to move {from:?} to temp flatten path {tmp_dir:?}"))?;

    if let Err(err) = fs::remove_dir_all(to) {
        let _ = fs::rename(&tmp_dir, &from);
        return Err(err).with_context(|| format!("failed to remove outer directory at {to:?}"));
    }

    if let Err(err) = fs::rename(&tmp_dir, to) {
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err(err).with_context(|| format!("failed to rename {tmp_dir:?} into {to:?}"));
    }

    case_insensitive_exists(to.join("skin.ini"))
}

fn case_insensitive_exists(path_with_file: impl AsRef<Path>) -> Result<bool> {
    let path_with_file = path_with_file.as_ref();

    // remove skin.ini from path
    let path = match path_with_file.ancestors().nth(1) {
        Some(path) => path,
        None => return Ok(false),
    };

    let file_name_as_lower = match path_with_file.file_name() {
        Some(name) => name.to_ascii_lowercase(),
        None => return Ok(false),
    };

    let folder =
        fs::read_dir(path).with_context(|| format!("failed to read directory at {path:?}"))?;

    for res in folder {
        let mut entry_as_lower = res
            .with_context(|| format!("failed to read entry of directory at {path:?}"))?
            .file_name();

        entry_as_lower.make_ascii_lowercase();

        if file_name_as_lower == entry_as_lower {
            return Ok(true);
        }
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::sanitize_skin_dir_name;

    #[test]
    fn test_sanitize_skin_dir_name() {
        assert_eq!(sanitize_skin_dir_name("...osk"), "skin");
        assert_eq!(sanitize_skin_dir_name("..osk"), "skin");
        assert_eq!(
            sanitize_skin_dir_name("My%20Custom%20Skin.osk"),
            "My Custom Skin"
        );
        assert_eq!(
            sanitize_skin_dir_name("../../etc/passwd.osk"),
            "etc_passwd"
        );
        assert_eq!(sanitize_skin_dir_name("clean_skin.ZIP"), "clean_skin");
    }
}
