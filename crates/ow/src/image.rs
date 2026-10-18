//! Golden images from code: a recipe (shell script) runs as root in a fresh
//! builder machine on this runner, and the builder's stopped disk becomes an
//! image that `ow create --image NAME` clones.
use crate::{ImageAction, common, selfhost, wire};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use ow_core::runtime::{GOLDEN_BASES, PROFILES};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const SCRIPT: &str = "/root/ow-recipe.sh";

pub fn run(base: Option<PathBuf>, action: ImageAction) -> Result<i32> {
    let runner = |dir: Option<PathBuf>| -> Result<PathBuf> {
        Ok(match dir {
            Some(dir) => dir,
            None => selfhost::base(base.clone())?.join("runner"),
        })
    };
    match action {
        ImageAction::Build {
            recipe,
            runner: dir,
            memory,
            cpus,
            timeout_minutes,
            publish_to,
            node,
            base_image,
        } => {
            if let Some(url) = publish_to {
                ensure!(dir.is_none(), "controller builds use --node, not --runner");
                return controller_build(
                    base,
                    &recipe,
                    &url,
                    node.as_deref().unwrap_or("local"),
                    base_image
                        .as_deref()
                        .context("controller builds need --base-image NAME@N")?,
                    memory,
                    cpus,
                    timeout_minutes,
                );
            }
            ensure!(base_image.is_none(), "--base-image requires --publish-to");
            let image = build(
                &runner(dir)?,
                &recipe,
                memory,
                cpus,
                Duration::from_secs(timeout_minutes * 60),
            )?;
            println!("{}", serde_json::to_string_pretty(&image)?);
        }
        ImageAction::Publish {
            name,
            to,
            runner: dir,
            base_image,
        } => {
            let controller = selfhost::base(base.clone())?.join("controller");
            let mut db = crate::image_catalog::open(&controller)?;
            let runner = runner(dir)?;
            let (disk, local) = sealed_source(&runner, &name)?;
            let disk_digest = local["disk_sha256"]
                .as_str()
                .context("disk seal required")?;
            let recipe_digest = local["recipe_sha256"].as_str().unwrap_or(disk_digest);
            let (base_name, base_digest) = if PROFILES.contains(&name.as_str()) {
                (name.clone(), disk_digest.to_owned())
            } else {
                let selector = base_image
                    .context("publishing a golden image requires --base-image NAME@N provenance")?;
                ensure!(
                    ow_core::store::image_name(&selector)?.1.is_some(),
                    "pin --base-image as NAME@N"
                );
                let pinned = crate::image_catalog::resolve(&db, &selector)?;
                ensure!(
                    pinned.store_url == to,
                    "golden base belongs to another store"
                );
                ensure!(
                    local["base"]
                        .as_str()
                        .is_some_and(|base| ow_core::store::image_name(base)
                            .is_ok_and(|(name, _)| name == pinned.name)),
                    "golden image base differs from --base-image"
                );
                (pinned.name, pinned.reference.digest)
            };
            let mut intent = crate::image_catalog::reserve(
                &mut db,
                &name,
                &base_name,
                &base_digest,
                recipe_digest,
                &to,
                "local",
                crate::image_store::now(),
            )?;
            if intent.status == "building" {
                let objects = ow_core::store::S3Objects::new(&to, None)?;
                objects.verify_configuration()?;
                let reference = crate::image_store::publish(&objects, &intent, &disk, local)?;
                let manifest = ow_core::store::read_manifest(&objects, &reference)?;
                intent = crate::image_catalog::commit(
                    &mut db,
                    &intent,
                    &reference,
                    &manifest,
                    crate::image_store::now(),
                )?;
            }
            println!("{}", serde_json::to_string_pretty(&intent)?);
        }
        ImageAction::Pull {
            image: selector,
            runner: dir,
            from,
            attempt,
            digest,
            base_assets,
        } => {
            let image = if let Some(url) = from {
                crate::image_store::from_reference(
                    &url,
                    &selector,
                    ow_core::store::Reference {
                        attempt: attempt.context("upload token required")?,
                        digest: digest.context("digest required")?,
                    },
                )?
            } else {
                let db =
                    crate::image_catalog::open(&selfhost::base(base.clone())?.join("controller"))?;
                crate::image_catalog::resolve(&db, &selector)?
            };
            let objects = ow_core::store::S3Objects::new(&image.store_url, None)?;
            let runner = runner(dir)?;
            let installed = crate::image_store::install(&objects, &image, &runner)?;
            if let Some(assets) = base_assets {
                crate::image_store::install_base(&objects, &image, &runner, &assets)?;
            }
            println!("{installed}");
        }
        ImageAction::List {
            runner: dir,
            catalog,
        } => {
            if catalog {
                let db = crate::image_catalog::open(&selfhost::base(base)?.join("controller"))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&crate::image_catalog::list(&db)?)?
                );
                return Ok(0);
            }
            let images = wire::request(&runner(dir)?, json!({"op":"images"}))?;
            println!("{}", serde_json::to_string_pretty(&images)?);
        }
    }
    Ok(0)
}

/// The image name (the file stem) and base profile of a recipe.
fn parse(path: &Path, text: &str) -> Result<(String, String)> {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("recipe file name must be UTF-8")?
        .to_owned();
    common::identifier(&name).context("recipe file name must be a valid image name")?;
    ensure!(
        !PROFILES.contains(&name.as_str()) && name != "ubuntu-arm64",
        "{name} is a built-in image; rename the recipe"
    );
    let base = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("# ow-image:"))
        .and_then(|h| h.split_whitespace().find_map(|kv| kv.strip_prefix("base=")))
        .context("recipe needs a header line such as `# ow-image: base=ubuntu`")?;
    ensure!(
        GOLDEN_BASES.contains(&base),
        "recipe base must be one of {GOLDEN_BASES:?}"
    );
    Ok((name, base.to_owned()))
}

fn build(runner: &Path, recipe: &Path, memory: u32, cpus: u32, timeout: Duration) -> Result<Value> {
    let text = fs::read_to_string(recipe)
        .with_context(|| format!("reading recipe {}", recipe.display()))?;
    ensure!(text.len() <= 192 * 1024, "recipes are limited to 192 KiB");
    let (name, base) = parse(recipe, &text)?;
    let digest: String = Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let built = wire::request(runner, json!({"op":"images"}))?;
    if let Some(same) = built.as_array().and_then(|a| {
        a.iter()
            .find(|i| i["name"] == name && i["recipe_sha256"] == digest)
    }) {
        eprintln!("{name} is already built from this recipe");
        return Ok(same.clone());
    }
    build_text(runner, &name, &base, &text, memory, cpus, timeout, None)
}

pub(crate) fn build_text(
    runner: &Path,
    name: &str,
    base: &str,
    text: &str,
    memory: u32,
    cpus: u32,
    timeout: Duration,
    builder_id: Option<&str>,
) -> Result<Value> {
    let digest = ow_core::store::sha256(text.as_bytes());
    let builder = builder_id
        .map(str::to_owned)
        .unwrap_or(format!("img-{}", &common::nonce()?[..16]));
    eprintln!("building {name} on {base} in machine {builder}");
    wire::request(
        runner,
        json!({"op":"create","id":builder,"image":base,"memory_mib":memory,"vcpu_count":cpus}),
    )
    .context("creating the builder machine")?;
    let result = (|| -> Result<Value> {
        execute(
            runner,
            &builder,
            &recipe_run(runner, &builder, &text)?,
            timeout,
        )?;
        let (_, code) = shell(runner, &builder, "rm -f /root/ow-recipe.*; sync")?;
        ensure!(
            code == 0,
            "could not clean and sync the builder before sealing"
        );
        wire::request(runner, json!({"op":"stop","id":builder}))?;
        wire::request(
            runner,
            json!({"op":"image-save","id":builder,"name":name,"recipe_sha256":digest}),
        )
    })();
    let _ = wire::request(runner, json!({"op":"stop","id":builder}));
    if let Err(e) = wire::request(runner, json!({"op":"delete","id":builder})) {
        eprintln!("removing builder machine {builder}: {e:#}");
    }
    result
}

fn shell(runner: &Path, id: &str, command: &str) -> Result<(String, i64)> {
    let value = wire::request(runner, json!({"op":"shell-exec","id":id,"command":command}))?;
    Ok((
        value["output"].as_str().unwrap_or_default().to_owned(),
        value["exit_code"].as_i64().unwrap_or(-1),
    ))
}

/// Uploads the recipe and returns the command that starts it in the background:
/// a management command is bounded to 30 seconds, a recipe is not. The usual
/// umask, not the management shell's 077, so files it installs are readable.
fn recipe_run(runner: &Path, id: &str, text: &str) -> Result<String> {
    wire::request(
        runner,
        json!({"op":"put","id":id,"path":SCRIPT,"data":STANDARD.encode(text)}),
    )?;
    Ok(format!(
        // In a subshell: the serial transport appends `; …` to the command.
        "(umask 022; cd /root && HOME=/root DEBIAN_FRONTEND=noninteractive nohup sh -c 'sh {SCRIPT} >/root/ow-recipe.log 2>&1; echo $? >/root/ow-recipe.exit' </dev/null >/dev/null 2>&1 &)"
    ))
}

fn execute(runner: &Path, id: &str, start: &str, timeout: Duration) -> Result<()> {
    let (_, code) = shell(runner, id, start)?;
    ensure!(code == 0, "starting the recipe failed ({code})");
    let started = Instant::now();
    let mut reported = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(3));
        let (output, _) = shell(runner, id, "cat /root/ow-recipe.exit 2>/dev/null")?;
        if let Ok(code) = output.trim().parse::<i32>() {
            if code == 0 {
                eprintln!("recipe finished in {}s", started.elapsed().as_secs());
                return Ok(());
            }
            let (log, _) = shell(runner, id, "tail -n 40 /root/ow-recipe.log")?;
            eprintln!("{log}");
            bail!("recipe exited with status {code}");
        }
        if started.elapsed() > timeout {
            let (log, _) = shell(runner, id, "tail -n 40 /root/ow-recipe.log")?;
            eprintln!("{log}");
            bail!("recipe still running after {}s", timeout.as_secs());
        }
        if reported.elapsed() > Duration::from_secs(30) {
            eprintln!("still running ({}s)", started.elapsed().as_secs());
            reported = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipes_name_the_image_and_its_base() {
        let p = Path::new("recipes/node-dev.sh");
        assert_eq!(
            parse(
                p,
                "#!/bin/sh\n# ow-image: base=ubuntu\napt-get install -y nodejs\n"
            )
            .unwrap(),
            ("node-dev".into(), "ubuntu".into())
        );
        assert!(
            parse(p, "#!/bin/sh\napt-get update\n").is_err(),
            "no header"
        );
        assert!(
            parse(p, "# ow-image: base=alpine\n").is_err(),
            "alpine base"
        );
        assert!(parse(Path::new("ubuntu.sh"), "# ow-image: base=ubuntu\n").is_err());
        assert!(parse(Path::new("bad name.sh"), "# ow-image: base=ubuntu\n").is_err());
    }
}

/// Source remains sealed throughout transfer; base assets use their computed disk seal.
fn sealed_source(runner: &Path, name: &str) -> Result<(PathBuf, Value)> {
    common::identifier(name)?;
    if PROFILES.contains(&name) {
        let path = crate::assets().join(if name == "alpine" {
            "guest/base.ext4".into()
        } else {
            format!("guest/{name}.ext4")
        });
        let disk_sha256 = ow_core::store::hash_file(&path)?;
        let profile = fs::read(crate::assets().join(format!("guest/{name}.json")))
            .ok()
            .map(|b| serde_json::from_slice::<Value>(&b))
            .transpose()?
            .unwrap_or(json!({}));
        let kernel = ow_core::store::hash_file(&crate::assets().join("downloads/vmlinux-6.1.186"))?;
        Ok((
            path,
            json!({"name":name,"base":name,"disk_sha256":disk_sha256,"profile":profile,"kernel_sha256":kernel}),
        ))
    } else {
        let path = runner.join("images").join(name);
        let manifest: Value = serde_json::from_slice(&fs::read(path.join("manifest.json"))?)?;
        ensure!(manifest["name"] == name, "local image name mismatch");
        Ok((path.join("disk.ext4"), manifest))
    }
}
fn controller_build(
    base: Option<PathBuf>,
    recipe: &Path,
    url: &str,
    node: &str,
    base_image: &str,
    memory: u32,
    cpus: u32,
    timeout_minutes: u64,
) -> Result<i32> {
    use crate::{image_catalog as catalog, image_store};
    common::resources(memory, cpus)?;
    ensure!(
        (1..=60).contains(&timeout_minutes),
        "controller builds allow 1–60 minutes"
    );
    let text = fs::read_to_string(recipe)?;
    ensure!(text.len() <= 192 * 1024, "recipes are limited to 192 KiB");
    let (name, base_name) = parse(recipe, &text)?;
    ensure!(
        ow_core::store::image_name(base_image)?.1.is_some(),
        "pin --base-image as NAME@N"
    );
    let controller = selfhost::base(base)?.join("controller");
    let mut db = catalog::open(&controller)?;
    let base_image = catalog::resolve(&db, base_image)?;
    ensure!(
        base_image.name == base_name && base_image.store_url == url,
        "recipe base or store differs from pinned base"
    );
    let image = catalog::reserve(
        &mut db,
        &name,
        &base_name,
        &base_image.reference.digest,
        &ow_core::store::sha256(text.as_bytes()),
        url,
        node,
        image_store::now(),
    )?;
    if image.status != "building" {
        println!("{}", serde_json::to_string_pretty(&image)?);
        return Ok(0);
    }
    let result = image_store::dispatch(
        &controller,
        &image.node,
        json!({"op":"image-build","image":image,"base_image":base_image,"recipe":text,"memory_mib":memory,"vcpu_count":cpus,"timeout_seconds":timeout_minutes*60}),
    )?;
    let reference: ow_core::store::Reference = serde_json::from_value(result["reference"].clone())?;
    let objects = ow_core::store::S3Objects::new(url, None)?;
    let manifest = ow_core::store::read_manifest(&objects, &reference)?;
    let current = catalog::commit(&mut db, &image, &reference, &manifest, image_store::now())?;
    println!("{}", serde_json::to_string_pretty(&current)?);
    Ok(0)
}
