use super::*;

pub(crate) fn load_attribute_icons(
    ctx: &egui::Context,
    root: &std::path::Path,
) -> HashMap<String, egui::TextureHandle> {
    ATTRIBUTE_ICON_PATHS
        .into_iter()
        .filter_map(|(attribute, path)| {
            load_image_texture(ctx, root, path, "attribute-icon")
                .map(|texture| (attribute.to_owned(), texture))
        })
        .collect()
}

pub(crate) fn load_monster_textures(
    ctx: &egui::Context,
    root: &std::path::Path,
    monster_ids: &[String],
) -> HashMap<String, egui::TextureHandle> {
    let mut textures = HashMap::new();
    for monster_id in monster_ids.iter().map(String::as_str) {
        for stem in monster_image_stem_candidates(monster_id) {
            let resource_keys = monster_image_resource_keys(&stem);
            if resource_keys.iter().any(|key| textures.contains_key(key)) {
                break;
            }
            let resource_path = format!("{MONSTER_IMAGE_DIR}/{stem}.png");
            let Some(texture) = load_image_texture(ctx, root, &resource_path, "monster") else {
                continue;
            };
            for key in resource_keys {
                textures.entry(key).or_insert_with(|| texture.clone());
            }
            break;
        }
    }

    textures
}

pub(crate) fn load_damage_digit_textures(
    ctx: &egui::Context,
    root: &std::path::Path,
) -> HashMap<String, Vec<egui::TextureHandle>> {
    let mut textures = HashMap::new();
    for (key, prefix) in DAMAGE_DIGIT_TEXTURE_SETS {
        let digits = (0..=9)
            .filter_map(|digit| {
                let path = damage_digit_resource_path(prefix, digit);
                load_image_texture(ctx, root, &path, "damage-digit")
            })
            .collect::<Vec<_>>();
        if digits.len() == 10 {
            textures.insert(key.to_owned(), digits);
        }
    }
    textures
}

pub(crate) fn load_reaction_text_textures(
    ctx: &egui::Context,
    root: &std::path::Path,
) -> HashMap<u8, Vec<egui::TextureHandle>> {
    let mut textures = HashMap::new();
    for reaction in 1..=REACTION_TEXT_IMAGE_COUNT {
        let glyphs = (1..=2)
            .filter_map(|part| {
                let path = format!("{DAMAGE_DIGIT_IMAGE_DIR}/fanying{reaction:02}_{part:02}.png");
                load_image_texture(ctx, root, &path, "reaction-text")
            })
            .collect::<Vec<_>>();
        if glyphs.len() == 2 {
            textures.insert(reaction, glyphs);
        }
    }
    textures
}

pub(crate) fn damage_digit_resource_path(prefix: &str, digit: usize) -> String {
    format!("{DAMAGE_DIGIT_IMAGE_DIR}/{prefix}_{digit}.png")
}

pub(crate) fn load_character_avatars(
    ctx: &egui::Context,
    root: &std::path::Path,
    characters: &HashMap<u32, CharacterInfo>,
) -> HashMap<String, egui::TextureHandle> {
    let mut textures = HashMap::new();
    for avatar in characters
        .values()
        .filter_map(|character| character.avatar.as_deref())
    {
        if textures.contains_key(avatar) {
            continue;
        }
        if let Some(texture) = load_image_texture(ctx, root, avatar, "character-avatar") {
            textures.insert(avatar.to_owned(), texture);
        }
    }
    textures
}

pub(crate) fn fill_missing_character_colors_from_avatars(
    characters: &mut HashMap<u32, CharacterInfo>,
    root: &std::path::Path,
) {
    let mut avatar_colors = HashMap::<String, Color32>::new();
    for character in characters.values_mut() {
        if character
            .color
            .as_deref()
            .and_then(parse_hex_color)
            .is_some()
        {
            continue;
        }
        let Some(avatar) = character.avatar.as_deref() else {
            continue;
        };
        let color = avatar_colors
            .entry(avatar.to_owned())
            .or_insert_with(|| {
                avatar_accent_color(root, avatar)
                    .unwrap_or_else(|| deterministic_character_fallback_color(avatar.as_bytes()))
            })
            .to_owned();
        character.color = Some(format!(
            "#{:02X}{:02X}{:02X}",
            color.r(),
            color.g(),
            color.b()
        ));
    }
}

pub(crate) fn avatar_accent_color(root: &std::path::Path, resource_path: &str) -> Option<Color32> {
    let path = root.join(resource_path);
    let bytes = std::fs::read(&path)
        .map(std::borrow::Cow::Owned)
        .or_else(|_| read_resource_bytes(Path::new(resource_path)))
        .ok()?;
    let image = image::load_from_memory(bytes.as_ref()).ok()?.to_rgba8();
    let mut red = 0.0_f64;
    let mut green = 0.0_f64;
    let mut blue = 0.0_f64;
    let mut total_weight = 0.0_f64;
    for pixel in image.pixels() {
        let [r, g, b, a] = pixel.0;
        if a < 128 {
            continue;
        }
        let rf = f64::from(r) / 255.0;
        let gf = f64::from(g) / 255.0;
        let bf = f64::from(b) / 255.0;
        let max = rf.max(gf).max(bf);
        let min = rf.min(gf).min(bf);
        let saturation = if max <= f64::EPSILON {
            0.0
        } else {
            (max - min) / max
        };
        if !(0.16..=0.96).contains(&max) || saturation < 0.16 {
            continue;
        }
        let mid_luma_weight = 1.0 - ((max - 0.58).abs() / 0.58).clamp(0.0, 0.85);
        let weight = saturation.powf(1.35) * mid_luma_weight.max(0.25) * f64::from(a) / 255.0;
        red += rf * weight;
        green += gf * weight;
        blue += bf * weight;
        total_weight += weight;
    }
    if total_weight <= f64::EPSILON {
        return None;
    }
    let mut r = red / total_weight;
    let mut g = green / total_weight;
    let mut b = blue / total_weight;
    let max = r.max(g).max(b).max(0.001);
    let min = r.min(g).min(b);
    let saturation = (max - min) / max;
    if saturation < 0.24 {
        let mean = (r + g + b) / 3.0;
        r = mean + (r - mean) * 1.45;
        g = mean + (g - mean) * 1.45;
        b = mean + (b - mean) * 1.45;
    }
    let max = r.max(g).max(b).max(0.001);
    if max < 0.46 {
        let scale = 0.46 / max;
        r *= scale;
        g *= scale;
        b *= scale;
    }
    Some(Color32::from_rgb(
        (r.clamp(0.0, 0.92) * 255.0).round() as u8,
        (g.clamp(0.0, 0.92) * 255.0).round() as u8,
        (b.clamp(0.0, 0.92) * 255.0).round() as u8,
    ))
}

pub(crate) fn deterministic_character_fallback_color(seed: &[u8]) -> Color32 {
    const PALETTE: [Color32; 12] = [
        Color32::from_rgb(193, 74, 105),
        Color32::from_rgb(112, 91, 179),
        Color32::from_rgb(70, 164, 126),
        Color32::from_rgb(210, 145, 62),
        Color32::from_rgb(72, 137, 195),
        Color32::from_rgb(171, 89, 178),
        Color32::from_rgb(92, 159, 220),
        Color32::from_rgb(219, 112, 85),
        Color32::from_rgb(128, 174, 73),
        Color32::from_rgb(210, 92, 145),
        Color32::from_rgb(87, 177, 166),
        Color32::from_rgb(154, 125, 218),
    ];
    let hash = seed.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    PALETTE[hash as usize % PALETTE.len()]
}

pub(crate) fn load_image_texture(
    ctx: &egui::Context,
    root: &std::path::Path,
    resource_path: &str,
    texture_namespace: &str,
) -> Option<egui::TextureHandle> {
    let path = root.join(resource_path);
    let bytes = std::fs::read(&path)
        .map(std::borrow::Cow::Owned)
        .or_else(|_| read_resource_bytes(Path::new(resource_path)))
        .ok()?;
    let image = image::load_from_memory(bytes.as_ref()).ok()?.to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    let color_image = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
    // Source art (e.g. 256px avatars) is drawn much smaller (~32px), an 8x
    // minification. Plain bilinear sampling has no mip chain, so it only reads a
    // 2x2 texel neighborhood and aliases hard edges into the jagged look seen on
    // screen. Trilinear filtering with generated mipmaps samples a pre-averaged
    // level, keeping shrunken images crisp and smooth. NOTE: only egui_glow honors this
    // (it calls `generate_mipmap`); egui-wgpu 0.34 uploads with `mip_level_count: 1` and
    // ignores `mipmap_mode`, so under the wgpu backend heavy minification falls back to
    // plain bilinear and aliases a little more. Pre-downscaling the source art would
    // restore smoothness if that ever looks bad. `mipmap_mode` is kept set so the glow
    // path (or a future wgpu mip chain) still benefits.
    let texture_options = egui::TextureOptions {
        magnification: egui::TextureFilter::Linear,
        minification: egui::TextureFilter::Linear,
        wrap_mode: egui::TextureWrapMode::ClampToEdge,
        mipmap_mode: Some(egui::TextureFilter::Linear),
    };
    Some(ctx.load_texture(
        format!("{texture_namespace}:{resource_path}"),
        color_image,
        texture_options,
    ))
}

pub(crate) fn pixel_aligned_rect(
    origin: egui::Pos2,
    logical_size: f32,
    pixels_per_point: f32,
) -> egui::Rect {
    let pixels_per_point = pixels_per_point.max(1.0);
    let physical_size = (logical_size * pixels_per_point).round();
    let size = physical_size / pixels_per_point;
    let min = egui::pos2(
        (origin.x * pixels_per_point).round() / pixels_per_point,
        (origin.y * pixels_per_point).round() / pixels_per_point,
    );
    egui::Rect::from_min_size(min, egui::vec2(size, size))
}

pub(crate) fn configure_style(ctx: &egui::Context, dark_mode: bool) {
    let mut visuals = if dark_mode {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    if dark_mode {
        visuals.panel_fill = Color32::from_rgb(9, 9, 11);
        visuals.window_fill = Color32::from_rgb(9, 9, 11);
        visuals.extreme_bg_color = Color32::from_rgb(9, 9, 11);
        visuals.faint_bg_color = Color32::from_rgb(24, 24, 27);
        visuals.code_bg_color = Color32::from_rgb(24, 24, 27);
    } else {
        visuals.panel_fill = Color32::from_rgb(255, 255, 255);
        visuals.window_fill = Color32::from_rgb(255, 255, 255);
        visuals.extreme_bg_color = Color32::from_rgb(250, 250, 250);
        visuals.faint_bg_color = Color32::from_rgb(244, 244, 245);
        visuals.code_bg_color = Color32::from_rgb(244, 244, 245);
    }
    let border = shadcn_border(dark_mode);
    let card = shadcn_card(dark_mode);
    let hover = shadcn_card_hover(dark_mode);
    visuals.widgets.noninteractive.bg_fill = Color32::TRANSPARENT;
    visuals.widgets.noninteractive.weak_bg_fill = Color32::TRANSPARENT;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, border);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(
        1.0,
        if dark_mode {
            Color32::from_rgb(250, 250, 250)
        } else {
            Color32::from_rgb(9, 9, 11)
        },
    );
    visuals.widgets.inactive.bg_fill = card;
    visuals.widgets.inactive.weak_bg_fill = card;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, border);
    visuals.widgets.inactive.fg_stroke = visuals.widgets.noninteractive.fg_stroke;
    visuals.widgets.hovered.bg_fill = hover;
    visuals.widgets.hovered.weak_bg_fill = hover;
    visuals.widgets.hovered.fg_stroke = visuals.widgets.noninteractive.fg_stroke;
    visuals.widgets.hovered.bg_stroke = Stroke::new(
        1.0,
        if dark_mode {
            Color32::from_rgb(63, 63, 70)
        } else {
            Color32::from_rgb(212, 212, 216)
        },
    );
    visuals.widgets.active.bg_fill = if dark_mode {
        Color32::from_rgb(82, 82, 91)
    } else {
        Color32::from_rgb(212, 212, 216)
    };
    visuals.widgets.active.weak_bg_fill = visuals.widgets.active.bg_fill;
    visuals.widgets.active.fg_stroke = Stroke::new(
        1.0,
        if dark_mode {
            Color32::from_rgb(250, 250, 250)
        } else {
            Color32::from_rgb(24, 24, 27)
        },
    );
    visuals.window_stroke = Stroke::new(1.0, border);
    let accent = theme_accent(dark_mode);
    visuals.selection.bg_fill = accent;
    visuals.selection.stroke = Stroke::new(1.0, contrast_text(accent));
    ctx.set_visuals(visuals);

    let mut style = (*ctx.global_style()).clone();
    style.animation_time = 0.14;
    style.interaction.selectable_labels = false;
    style.spacing.item_spacing = egui::vec2(8.0, 5.0);
    style.spacing.interact_size.y = INLINE_CONTROL_HEIGHT;
    style.spacing.button_padding = egui::vec2(11.0, 4.0);
    let mut scroll = egui::style::ScrollStyle::solid();
    scroll.bar_width = 8.0;
    scroll.handle_min_length = 32.0;
    scroll.bar_inner_margin = 4.0;
    scroll.bar_outer_margin = 2.0;
    scroll.foreground_color = true;
    style.spacing.scroll = scroll;
    style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(6);
    style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(6);
    style.visuals.widgets.hovered.expansion = 0.0;
    style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(6);
    style.visuals.widgets.active.expansion = 0.0;
    style.visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(6);
    ctx.set_global_style(style);
}
