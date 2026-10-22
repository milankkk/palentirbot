use eyre::{Context, Report, Result};
use std::{
    ffi::OsString,
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
};
use zip::ZipArchive;

use crate::{
    core::{BotConfig, Context as TwilightContext},
    util::{
        builder::MessageBuilder,
        constants::GENERAL_ISSUE,
        interaction::InteractionCommand,
        Authored, InteractionCommandExt,
    },
};

use super::SkinAdd;

fn is_private_or_local_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
        }
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
    }
}

pub async fn add(
    ctx: Arc<TwilightContext>,
    command: InteractionCommand,
    args: SkinAdd,
) -> Result<()> {
    let config = BotConfig::get();

    let user = match command.user() {
        Ok(user) => user,
        Err(err) => {
            command.error_callback(&ctx, GENERAL_ISSUE, false).await?;
            return Err(err.wrap_err("failed to get user from command"));
        }
    };

    if !config.owners.contains(&user.id) {
        command
            .error_callback(&ctx, crate::util::constants::NOT_OWNER, true)
            .await?;
        return Ok(());
    }

    let SkinAdd { skin, url } = args;

    let (bytes, filename) = match (skin, url) {
        (Some(skin), None) => {
            let filename = match skin.filename.rsplit_once('.') {
                Some((filename, "osk")) => filename.to_string(),
                _ => {
                    let content = "The attachment must be a .osk file!";
                    command.error_callback(&ctx, content, true).await?;
                    return Ok(());
                }
            };
            let mut filename = crate::commands::danser::render_score::sanitize_filename(&filename);
            if filename.is_empty() {
                filename = "skin".to_string();
            }
            let builder = MessageBuilder::new().embed("Downloading...");
            command.callback(&ctx, builder, false).await?;
            let bytes = match ctx.client().get_discord_attachment(&skin).await {
                Ok(bytes) => bytes,
                Err(err) => {
                    let _ = command.error(&ctx, "Failed to download attachment").await;
                    return Err(err.wrap_err("failed to download skin attachment"));
                }
            };
            (bytes, filename)
        }
        (None, Some(url)) => {
            if !url.ends_with(".osk") {
                command
                    .error_callback(&ctx, "The URL must point to a .osk file!", true)
                    .await?;
                return Ok(());
            }

            let parsed = match reqwest::Url::parse(&url) {
                Ok(u) => u,
                Err(_) => {
                    command.error_callback(&ctx, "Invalid URL!", true).await?;
                    return Ok(());
                }
            };

            if parsed.scheme() != "http" && parsed.scheme() != "https" {
                command
                    .error_callback(&ctx, "URL must use http or https scheme!", true)
                    .await?;
                return Ok(());
            }

            if let Some(host) = parsed.host_str() {
                let h = host.to_ascii_lowercase();
                let is_blocked = if h == "localhost" {
                    true
                } else if let Ok(ip) = h.parse::<std::net::IpAddr>() {
                    is_private_or_local_ip(ip)
                } else if let Ok(mut addrs) = tokio::net::lookup_host(format!("{h}:443")).await {
                    addrs.any(|addr| is_private_or_local_ip(addr.ip()))
                } else {
                    false
                };

                if is_blocked {
                    command
                        .error_callback(
                            &ctx,
                            "Local or private network addresses are not allowed!",
                            true,
                        )
                        .await?;
                    return Ok(());
                }
            }

            let builder = MessageBuilder::new().embed("Downloading from URL...");
            command.callback(&ctx, builder, false).await?;

            let bytes = match ctx.client().get_skin_from_url(&url).await {
                Ok(b) => b,
                Err(err) => {
                    let _ = command.error(&ctx, "Failed to download from URL").await;
                    return Err(Report::from(err).wrap_err("failed to download skin from url"));
                }
            };

            let raw_filename = url
                .split('/')
                .last()
                .unwrap_or("skin")
                .trim_end_matches(".osk");
            let decoded_filename = if let Some(pos) = raw_filename.find('?') {
                &raw_filename[..pos]
            } else {
                raw_filename
            };
            let mut filename = crate::commands::danser::render_score::sanitize_filename(decoded_filename);
            if filename.is_empty() {
                filename = "skin".to_string();
            }

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

    let mut builder = MessageBuilder::new().embed("Zipping...");
    command.update(&ctx, &builder).await?;

    let cursor = Cursor::new(bytes);

    let mut archive = match ZipArchive::new(cursor) {
        Ok(archive) => archive,
        Err(err) => {
            let _ = command.error(&ctx, GENERAL_ISSUE).await;
            let err = Report::from(err).wrap_err("failed to create zip archive");

            return Err(err);
        }
    };

    // Slight optimization by re-using the builder and overwriting the previous embed
    builder = builder.embed("Generating skin name...");
    command.update(&ctx, &builder).await?;

    let mut skin_file = BotConfig::get().paths.skins();

    let skin_list: Vec<_> = ctx
        .skin_list()
        .get()?
        .iter()
        .map(|skin| skin.to_ascii_lowercase())
        .collect();

    let mut needle = OsString::from(filename.clone());
    needle.make_ascii_lowercase();

    let idx = match skin_list.binary_search(&needle) {
        Ok(idx) => {
            let mut suffix = 1;

            loop {
                skin_file.push(format!("{filename}_{suffix}"));

                if !skin_file.exists() {
                    break idx + suffix + 1;
                }

                skin_file.pop();
                suffix += 1;
            }
        }
        Err(idx) => {
            skin_file.push(filename);

            idx + 1
        }
    };

    builder = builder.embed("Extracting...");
    command.update(&ctx, &builder).await?;

    const MAX_UNCOMPRESSED_SKIN_BYTES: u64 = 400 * 1024 * 1024; // 400 MB
    let skin_file_clone = skin_file.clone();
    let extract_res = tokio::task::spawn_blocking(move || -> Result<(), Report> {
        let mut total_size: u64 = 0;
        for i in 0..archive.len() {
            let file = archive
                .by_index(i)
                .map_err(|e| Report::from(e).wrap_err("corrupt zip entry in skin archive"))?;
            total_size += file.size();
            if total_size > MAX_UNCOMPRESSED_SKIN_BYTES {
                eyre::bail!("Skin archive exceeds 400MB uncompressed limit!");
            }
        }
        archive
            .extract(&skin_file_clone)
            .map_err(|e| Report::from(e).wrap_err("failed to extract zip archive"))?;
        Ok(())
    })
    .await;

    match extract_res {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            let msg = err.to_string();
            let _ = command.error(&ctx, msg).await;
            return Err(err);
        }
        Err(err) => {
            let _ = command.error(&ctx, GENERAL_ISSUE).await;
            return Err(Report::from(err).wrap_err("extraction task panicked"));
        }
    }

    let mut skin_ini_path = skin_file.clone();
    skin_ini_path.push("skin.ini");

    if !(case_insensitive_exists(skin_ini_path)? || move_directory(&skin_file)?) {
        let content = "There was an error getting the folder containing the skin elements! \
            Try re-exporting the skin!";
        command.error(&ctx, content).await?;

        fs::remove_dir_all(&skin_file).with_context(|| {
            format!("failed to remove directory after failing to assure skin.ini for {skin_file:?}")
        })?;

        return Ok(());
    }

    // Reset the skin list cache
    ctx.skin_list().clear();

    let content = format!("Added skin to list at index `{idx}`");
    builder = builder.embed(content);
    command.update(&ctx, &builder).await?;

    Ok(())
}

fn move_directory(to: &PathBuf) -> Result<bool> {
    let mut skin_folder =
        fs::read_dir(to).with_context(|| format!("failed to read directory at {to:?}"))?;

    let inner_folder = match skin_folder.next() {
        Some(res) => res.with_context(|| format!("failed to read entry of directory at {to:?}"))?,
        None => return Ok(false),
    };

    let from = inner_folder.path();

    let mut skin_ini = to.to_owned();
    skin_ini.push("skin.ini");

    if copy_all(from, to).is_ok() && case_insensitive_exists(&skin_ini)? {
        fs::remove_dir_all(inner_folder.path())
            .with_context(|| format!("failed to remove directory at {:?}", inner_folder.path()))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

#[allow(clippy::ptr_arg)]
fn copy_all(from: PathBuf, to: &PathBuf) -> Result<()> {
    let output_root = to;
    let input_root = from.components().count();

    let mut stack = vec![from];

    while let Some(working_path) = stack.pop() {
        // Generate a relative path
        let src: PathBuf = working_path.components().skip(input_root).collect();

        // Create a destination if missing
        let dest = if src.components().count() == 0 {
            output_root.clone()
        } else {
            output_root.join(&src)
        };

        if !dest.exists() {
            fs::create_dir_all(&dest)
                .with_context(|| format!("failed to create directory {dest:?}"))?;
        }

        let folder = fs::read_dir(&working_path)
            .with_context(|| format!("failed to read directory at {working_path:?}"))?;

        for entry in folder {
            let path = entry
                .with_context(|| format!("failed to read entry of directory at {working_path:?}"))?
                .path();

            if path.is_dir() {
                stack.push(path);
            } else if let Some(filename) = path.file_name() {
                let dest_path = dest.join(filename);

                fs::copy(&path, &dest_path)
                    .with_context(|| format!("failed to copy {path:?} to {dest_path:?}"))?;
            }
        }
    }

    Ok(())
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
