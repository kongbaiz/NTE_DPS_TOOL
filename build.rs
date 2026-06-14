use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const IMAGE_EXTENSIONS: &[&str] = &["jpeg", "jpg", "png", "webp"];

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let resource_dir = manifest_dir.join("res");
    let icon_path = manifest_dir.join("res/icons/app-icon.ico");
    let output_path = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("embedded_resources.rs");

    println!("cargo:rerun-if-changed={}", resource_dir.display());
    println!("cargo:rerun-if-changed={}", icon_path.display());

    let mut images = Vec::new();
    collect_images(&resource_dir, &mut images);
    images.sort();

    let mut generated = String::from(
        "fn embedded_image_resource(path: &str) -> Option<&'static [u8]> {\n\
         \x20   let normalized = path.replace('\\\\', \"/\");\n\
         \x20   match normalized.as_str() {\n",
    );

    for image in images {
        println!("cargo:rerun-if-changed={}", image.display());
        let relative = image
            .strip_prefix(&manifest_dir)
            .expect("resource must be inside the project")
            .to_string_lossy()
            .replace('\\', "/");
        let absolute = image.to_string_lossy();
        generated.push_str(&format!(
            "        {relative:?} => Some(include_bytes!({absolute:?})),\n"
        ));
    }

    generated.push_str("        _ => None,\n    }\n}\n");
    fs::write(output_path, generated).expect("failed to generate embedded resource map");

    #[cfg(windows)]
    winresource::WindowsResource::new()
        .set_icon(icon_path.to_str().expect("icon path must be valid UTF-8"))
        .compile()
        .expect("failed to embed Windows application icon");
}

fn collect_images(directory: &Path, images: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };

    for entry in entries {
        let entry = entry.expect("failed to read res directory entry");
        let path = entry.path();
        if path.is_dir() {
            collect_images(&path, images);
            continue;
        }

        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        if extension
            .as_deref()
            .is_some_and(|extension| IMAGE_EXTENSIONS.contains(&extension))
        {
            images.push(path);
        }
    }
}
