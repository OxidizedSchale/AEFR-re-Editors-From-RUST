// 版权所有 (C) 2026 黛 (Dye) & AEFR Contributors
// 该程序是自由软件：您可以自由分发和/或修改它
// 它遵循由GNU通用公共许可证所规定的
// 自由软件基金会发布的许可证第3版
// 本程序的发布旨在希望它能对他人有所帮助，
// 但不提供任何保证；甚至不包括默示保证
// 商品适销性或适用于特定用途
// 详情请参阅GNU通用公共许可证
// GPL-3.0 License
//

use eframe::egui;
use egui::{
    epaint::Vertex, Color32, FontData, FontDefinitions, FontFamily, Mesh, Pos2, Rect, Shape,
    TextureHandle, TextureId, Vec2,
};
use rayon::prelude::*;
use rusty_spine::{
    AnimationState, AnimationStateData, Atlas, Skeleton, SkeletonJson, Slot,
};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

// ============================================================================
// 1. 智能多平台入口 (Smart Entry Points)
// ============================================================================

// [场景 A] 桌面端 (Windows/Linux/macOS)
// Termux 虽然是 Android，但走 cargo run 时，我们希望它像 Linux 一样运行
#[cfg(not(target_os = "android"))]
fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
            .with_title("AEFR - Desktop"),
        ..Default::default()
    };
    eframe::run_native(
        "AEFR_App",
        options,
        Box::new(|cc| Box::new(AefrApp::new(cc))),
    )
}

// [场景 B] Termux (Android环境下的 cargo run)
// Termux 环境下 target_os 是 android，但没有 android_app 上下文。
// eframe 允许 android_app 为 None，此时它会尝试使用 winit 的 X11/Wayland 后端。
#[cfg(target_os = "android")]
fn main() -> Result<(), eframe::Error> {
    use eframe::NativeOptions;
    let options = NativeOptions {
        android_app: None, // Termux 直接运行时没有 Activity
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
            .with_title("AEFR - Termux X11"),
        ..Default::default()
    };
    eframe::run_native(
        "AEFR_App",
        options,
        Box::new(|cc| Box::new(AefrApp::new(cc))),
    )
}

// [场景 C] Android APK (cargo-apk build)
// 这是给打包成 APP 用的入口
#[cfg(target_os = "android")]
use eframe::android_activity::AndroidApp;

#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: AndroidApp) {
    use eframe::NativeOptions;
    let options = NativeOptions {
        android_app: Some(app), // APK 运行时必须传入 app
        ..Default::default()
    };
    eframe::run_native(
        "AEFR_App",
        options,
        Box::new(|cc| Box::new(AefrApp::new(cc))),
    ).unwrap();
}

// ============================================================================
// 2. 指令系统
// ============================================================================

#[derive(Debug)]
enum AppCommand {
    Dialogue { name: String, affiliation: String, content: String },
    RequestLoad { slot_idx: usize, path: String },
    LoadSuccess(usize, Box<SpineObject>),
    LoadBackground(String),
    Log(String),
}

// ============================================================================
// 3. Spine 渲染核心
// ============================================================================

pub struct SpineObject {
    skeleton: Skeleton,
    state: AnimationState,
    _texture: TextureHandle,
    texture_id: TextureId,
    pub position: Pos2,
    pub scale: f32,
}

impl std::fmt::Debug for SpineObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpineObject").field("pos", &self.position).finish()
    }
}

unsafe impl Send for SpineObject {}

impl SpineObject {
    fn load_async(ctx: &egui::Context, path_str: &str) -> Option<Self> {
        let atlas_path = std::path::Path::new(path_str);
        let atlas = std::sync::Arc::new(Atlas::new_from_file(atlas_path).ok()?);

        let (texture_handle, texture_id) = if let Some(page) = atlas.pages().next() {
            let img_path = atlas_path.parent()?.join(page.name());
            let img = image::open(&img_path).ok()?;
            let size = [img.width() as usize, img.height() as usize];
            let rgba8 = img.to_rgba8();
            let color_image = egui::ColorImage::from_rgba_unmultiplied(size, rgba8.as_flat_samples().as_slice());
            let handle = ctx.load_texture(page.name(), color_image, egui::TextureOptions::LINEAR);
            
            // --- 修复点 1: 先获取 ID，再移动 handle ---
            let id = handle.id();
            (handle, id) 
        } else { return None; };

        let json_path = atlas_path.with_extension("json");
        let skeleton_json = SkeletonJson::new(atlas);
        let skeleton_data = std::sync::Arc::new(skeleton_json.read_skeleton_data_file(json_path).ok()?);
        let state_data = std::sync::Arc::new(AnimationStateData::new(skeleton_data.clone()));
        
        let mut state = AnimationState::new(state_data);
        
        // --- 修复点 2: 解决借用冲突 ---
        // 我们不从 state.data() 获取动画，而是直接从我们手里的 skeleton_data 获取
        // 这样就避开了同时借用 state (mutable) 和 state.data() (immutable) 的问题
        if let Some(anim) = skeleton_data.animations().next() {
            let _ = state.set_animation(0, &anim, true); 
        }

        Some(Self {
            skeleton: Skeleton::new(skeleton_data),
            state,
            _texture: texture_handle,
            texture_id,
            position: Pos2::new(0.0, 0.0),
            scale: 0.5,
        })
    }

    fn update_parallel(&mut self, dt: f32) {
        self.state.update(dt);
        self.state.apply(&mut self.skeleton);
        self.skeleton.update_world_transform();
    }

    fn paint(&self, ui: &mut egui::Ui) {
        let mut mesh = Mesh::with_texture(self.texture_id);
        let mut world_vertices = Vec::with_capacity(1024); 
        
        for slot in self.skeleton.draw_order() {
            if let Some(attachment) = slot.attachment() {
                if let Some(region) = attachment.as_region() {
                    unsafe {
                        if world_vertices.len() < 8 { world_vertices.resize(8, 0.0); }
                        region.compute_world_vertices(&*slot, &mut world_vertices, 0, 2);
                        let uvs = region.uvs();
                        self.push_to_mesh(&mut mesh, &world_vertices[0..8], &uvs, &[0, 1, 2, 2, 3, 0], &*slot, region.color());
                    }
                } else if let Some(mesh_att) = attachment.as_mesh() {
                    unsafe {
                        let len = mesh_att.world_vertices_length() as usize;
                        if world_vertices.len() < len { world_vertices.resize(len, 0.0); }
                        mesh_att.compute_world_vertices(&*slot, 0, len as i32, &mut world_vertices, 0, 2);
                        
                        let uvs_slice = std::slice::from_raw_parts(mesh_att.uvs(), len);
                        let tris_slice = std::slice::from_raw_parts(mesh_att.triangles(), mesh_att.triangles_count() as usize);

                        self.push_to_mesh(&mut mesh, &world_vertices[0..len], uvs_slice, tris_slice, &*slot, mesh_att.color());
                    }
                }
            }
        }
        ui.painter().add(Shape::mesh(mesh));
    }

    fn push_to_mesh(&self, mesh: &mut Mesh, w_v: &[f32], uvs: &[f32], tris: &[u16], slot: &Slot, att_c: rusty_spine::Color) {
        let s_c = slot.color();
        let color = Color32::from_rgba_premultiplied(
            (s_c.r * att_c.r * 255.0) as u8, (s_c.g * att_c.g * 255.0) as u8,
            (s_c.b * att_c.b * 255.0) as u8, (s_c.a * att_c.a * 255.0) as u8,
        );
        let idx_offset = mesh.vertices.len() as u32;
        let count = usize::min(uvs.len() / 2, w_v.len() / 2);
        
        for i in 0..count {
            let pos = Pos2::new(
                w_v[i * 2] * self.scale + self.position.x,
                -w_v[i * 2 + 1] * self.scale + self.position.y,
            );
            mesh.vertices.push(Vertex {
                pos,
                uv: Pos2::new(uvs[i * 2], uvs[i * 2 + 1]),
                color,
            });
        }
        for &idx in tris { mesh.indices.push(idx_offset + idx as u32); }
    }
}

// ============================================================================
// 4. 应用逻辑
// ============================================================================

struct AefrApp {
    current_name: String,
    current_affiliation: String,
    current_content: String,
    characters: Vec<Option<SpineObject>>,
    background: Option<TextureHandle>,
    tx: Sender<AppCommand>,
    rx: Receiver<AppCommand>,
    console_open: bool,
    console_input: String,
    console_logs: Vec<String>,
}

impl AefrApp {
    fn new(cc: &eframe::CreationContext) -> Self {
        setup_custom_fonts(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let (tx, rx) = channel();
        Self {
            current_name: "系统".into(),
            current_affiliation: "AEFR Editor".into(),
            current_content: "AEFR v0.5.5 Stable.\nCMD is ready.".into(),
            characters: vec![None, None, None, None, None],
            background: None,
            tx, rx,
            console_open: false,
            console_input: String::new(),
            console_logs: vec!["Ready.".into()],
        }
    }

    fn parse_and_send_command(&mut self) {
        let input = self.console_input.trim().to_owned();
        if input.is_empty() { return; }
        self.console_logs.push(format!("> {}", input));

        if let Some(rest) = input.strip_prefix("LOAD ") {
            let parts: Vec<&str> = rest.splitn(2, ' ').collect();
            if parts.len() == 2 {
                if let Ok(idx) = parts[0].parse::<usize>() {
                    let _ = self.tx.send(AppCommand::RequestLoad { slot_idx: idx, path: parts[1].replace("\"", "") });
                }
            }
        } else if let Some(rest) = input.strip_prefix("TALK ") {
            let p: Vec<&str> = rest.split('|').collect();
            if p.len() == 3 {
                let _ = self.tx.send(AppCommand::Dialogue { name: p[0].to_owned(), affiliation: p[1].to_owned(), content: p[2].to_owned() });
            }
        } else if let Some(path) = input.strip_prefix("BG ") {
            let _ = self.tx.send(AppCommand::LoadBackground(path.replace("\"", "")));
        } else if input.eq_ignore_ascii_case("HELP") {
            self.console_logs.push("LOAD <0-4> <path> | BG <path> | TALK <name>|<aff>|<msg>".into());
        }
        self.console_input.clear();
    }

    fn handle_async_events(&mut self, ctx: &egui::Context) {
        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                AppCommand::Dialogue { name, affiliation, content } => { 
                    self.current_name = name; 
                    self.current_affiliation = affiliation; 
                    self.current_content = content; 
                }
                AppCommand::Log(msg) => self.console_logs.push(msg),
                AppCommand::RequestLoad { slot_idx, path } => {
                    let tx_cb = self.tx.clone();
                    let ctx_clone = ctx.clone();
                    self.console_logs.push(format!("Loading slot {}...", slot_idx));
                    thread::spawn(move || {
                        if let Some(obj) = SpineObject::load_async(&ctx_clone, &path) {
                            let _ = tx_cb.send(AppCommand::LoadSuccess(slot_idx, Box::new(obj)));
                        } else {
                            let _ = tx_cb.send(AppCommand::Log(format!("Load failed: {}", path)));
                        }
                    });
                }
                AppCommand::LoadSuccess(idx, obj) => {
                    if idx < 5 { 
                        let mut loaded = *obj;
                        loaded.position = Pos2::new(200.0 + idx as f32 * 200.0, 720.0);
                        self.characters[idx] = Some(loaded); 
                    }
                }
                AppCommand::LoadBackground(path) => {
                    if let Ok(img) = image::open(&path) {
                        let rgba = img.to_rgba8();
                        let c_img = egui::ColorImage::from_rgba_unmultiplied([img.width() as _, img.height() as _], rgba.as_flat_samples().as_slice());
                        self.background = Some(ctx.load_texture("bg", c_img, egui::TextureOptions::LINEAR));
                    }
                }
            }
        }
    }
}

impl eframe::App for AefrApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_async_events(ctx);

        let dt = ctx.input(|i| i.stable_dt);
        self.characters.par_iter_mut().for_each(|slot| {
            if let Some(char) = slot { char.update_parallel(dt); }
        });
        if self.characters.iter().any(|c| c.is_some()) { ctx.request_repaint(); }

        egui::CentralPanel::default().show(ctx, |ui| {
            let screen_rect = ui.max_rect();
            if let Some(bg) = &self.background {
                ui.painter().image(bg.id(), screen_rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            }
            for char in self.characters.iter().flatten() { char.paint(ui); }
            draw_dialogue_ui(ui, screen_rect, &self.current_name, &self.current_affiliation, &self.current_content);

            let cmd_rect = Rect::from_min_size(Pos2::new(10.0, 10.0), Vec2::new(60.0, 40.0));
            if ui.put(cmd_rect, egui::Button::new("CMD")).clicked() { self.console_open = !self.console_open; }

            if self.console_open {
                egui::Window::new("CONSOLE").default_size([400.0, 300.0]).show(ctx, |ui| {
                    egui::ScrollArea::vertical().stick_to_bottom(true).max_height(200.0).show(ui, |ui| {
                        for l in &self.console_logs { ui.monospace(l); }
                    });
                    ui.horizontal(|ui| {
                        if ui.text_edit_singleline(&mut self.console_input).lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            self.parse_and_send_command();
                        }
                    });
                });
            }
        });
    }
}

fn draw_dialogue_ui(ui: &mut egui::Ui, screen: Rect, name: &str, affiliation: &str, content: &str) {
    let box_h = 160.0;
    let box_rect = Rect::from_min_max(Pos2::new(0.0, screen.bottom() - box_h), screen.max);
    ui.painter().rect_filled(box_rect, 0.0, Color32::from_black_alpha(180));
    
    if !name.is_empty() {
        let name_pos = box_rect.left_top() + egui::vec2(100.0, -25.0);
        ui.painter().rect_filled(Rect::from_min_size(name_pos, egui::vec2(200.0, 40.0)), 2.0, Color32::WHITE);
        ui.painter().rect_filled(Rect::from_min_max(name_pos, name_pos + egui::vec2(5.0, 40.0)), 2.0, Color32::from_rgb(0, 159, 232));
        ui.painter().text(name_pos + egui::vec2(15.0, 20.0), egui::Align2::LEFT_CENTER, format!("{} [{}]", name, affiliation), egui::FontId::proportional(20.0), Color32::BLACK);
    }
    ui.painter().text(box_rect.left_top() + egui::vec2(100.0, 50.0), egui::Align2::LEFT_TOP, content, egui::FontId::proportional(26.0), Color32::WHITE);
}

fn setup_custom_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let paths = vec!["/system/fonts/NotoSansCJK-Regular.ttc", "C:\\Windows\\Fonts\\msyh.ttc"];
    for p in paths {
        if let Ok(d) = std::fs::read(p) {
            fonts.font_data.insert("sys".into(), FontData::from_owned(d));
            fonts.families.get_mut(&FontFamily::Proportional).unwrap().insert(0, "sys".into());
            ctx.set_fonts(fonts);
            return;
        }
    }
  }
