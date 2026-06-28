use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use image::ImageEncoder;
use image::codecs::webp::WebPEncoder;
use serde_json::Value;

const EXCLUDED_EMBEDDED_RESOURCES: &[&str] = &[
    "res/data/asset_manifest.json",
    "res/data/asset_report.json",
    "res/icons/app-icon.ico",
];

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let resource_dir = manifest_dir.join("res");
    let icon_path = manifest_dir.join("res/icons/app-icon.ico");
    let output_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed={}", resource_dir.display());
    println!("cargo:rerun-if-changed={}", icon_path.display());
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EXTERNAL_RESOURCES");

    generate_embedded_resources(&manifest_dir, &resource_dir, &output_dir);

    #[cfg(windows)]
    winresource::WindowsResource::new()
        .set_icon(icon_path.to_str().expect("icon path must be valid UTF-8"))
        .compile()
        .expect("failed to embed Windows application icon");
}

fn generate_embedded_resources(manifest_dir: &Path, resource_dir: &Path, output_dir: &Path) {
    if env::var_os("CARGO_FEATURE_EXTERNAL_RESOURCES").is_some() {
        let generated = concat!(
            "fn embedded_resource(_path: &str) -> Option<&'static [u8]> {\n",
            "    None\n",
            "}\n",
        );
        let output_path = output_dir.join("embedded_resources.rs");
        fs::write(output_path, generated).expect("failed to generate embedded resource map");
        return;
    }

    let mut resources = Vec::new();
    collect_resources(resource_dir, &mut resources);
    resources.sort();
    let transformed_dir = output_dir.join("embedded_resources");
    if transformed_dir.exists() {
        fs::remove_dir_all(&transformed_dir)
            .expect("failed to clean generated embedded resource directory");
    }
    fs::create_dir_all(&transformed_dir)
        .expect("failed to create generated embedded resource directory");

    let mut generated = String::from(concat!(
        "fn embedded_resource(path: &str) -> Option<&'static [u8]> {\n",
        "  let normalized = path.replace('\\\\', \"/\");\n",
        "  match normalized.as_str() {\n",
    ));

    let mut original_bytes = 0_u64;
    let mut embedded_bytes = 0_u64;
    let mut skipped = 0_usize;
    let mut minified_json = 0_usize;
    let mut webp_images = 0_usize;

    for resource in resources {
        println!("cargo:rerun-if-changed={}", resource.display());
        let relative = resource
            .strip_prefix(manifest_dir)
            .expect("resource must be inside the project")
            .to_string_lossy()
            .replace('\\', "/");
        if should_exclude_embedded_resource(&relative) {
            skipped += 1;
            continue;
        }
        let original = fs::read(&resource).expect("failed to read resource for embedding");
        original_bytes += original.len() as u64;
        let processed = process_embedded_resource(&relative, &original);
        match processed.kind {
            EmbeddedResourceKind::Json => minified_json += 1,
            EmbeddedResourceKind::Webp => webp_images += 1,
            EmbeddedResourceKind::Original => {}
        }
        embedded_bytes += processed.bytes.len() as u64;
        let generated_path =
            transformed_dir.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = generated_path.parent() {
            fs::create_dir_all(parent).expect("failed to create generated resource parent");
        }
        fs::write(&generated_path, &processed.bytes)
            .expect("failed to write generated embedded resource");
        let absolute = generated_path.to_string_lossy();
        generated.push_str(&format!(
            "        {relative:?} => Some(include_bytes!({absolute:?})),\n"
        ));
    }

    generated.push_str("        _ => None,\n    }\n}\n");
    let output_path = output_dir.join("embedded_resources.rs");
    fs::write(output_path, generated).expect("failed to generate embedded resource map");
    if env::var_os("NTE_EMBEDDED_RESOURCE_REPORT").is_some() {
        println!(
            "cargo:warning=embedded resources: skipped {skipped}, minified_json {minified_json}, webp_images {webp_images}, bytes {} -> {}",
            original_bytes, embedded_bytes
        );
    }
}

fn collect_resources(directory: &Path, resources: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };

    for entry in entries {
        let entry = entry.expect("failed to read res directory entry");
        let path = entry.path();
        if path.is_dir() {
            collect_resources(&path, resources);
            continue;
        }

        resources.push(path);
    }
}

fn should_exclude_embedded_resource(relative: &str) -> bool {
    EXCLUDED_EMBEDDED_RESOURCES
        .iter()
        .any(|excluded| relative.eq_ignore_ascii_case(excluded))
}

struct EmbeddedResource {
    bytes: Vec<u8>,
    kind: EmbeddedResourceKind,
}

enum EmbeddedResourceKind {
    Original,
    Json,
    Webp,
}

fn process_embedded_resource(relative: &str, original: &[u8]) -> EmbeddedResource {
    if relative.ends_with(".json")
        && let Ok(document) = serde_json::from_slice::<Value>(original)
    {
        return EmbeddedResource {
            bytes: serde_json::to_vec(&document).expect("failed to minify JSON resource"),
            kind: EmbeddedResourceKind::Json,
        };
    }

    if relative.ends_with(".png")
        && let Some(webp) = png_to_lossless_webp(original)
        && webp.len() < original.len()
    {
        return EmbeddedResource {
            bytes: webp,
            kind: EmbeddedResourceKind::Webp,
        };
    }

    EmbeddedResource {
        bytes: original.to_vec(),
        kind: EmbeddedResourceKind::Original,
    }
}

fn png_to_lossless_webp(original: &[u8]) -> Option<Vec<u8>> {
    let image = image::load_from_memory(original).ok()?.to_rgba8();
    let (width, height) = image.dimensions();
    let mut bytes = Vec::new();
    WebPEncoder::new_lossless(&mut bytes)
        .write_image(
            image.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(bytes)
}
