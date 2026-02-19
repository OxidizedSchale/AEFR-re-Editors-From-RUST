/*
 * Project: AEFR (AEFR's Eternal Freedom & Rust-rendered)
 * GitHub: https://github.com/OxidizedSchale/AEFR-s-Eternal-Freedom-Rust-rendered
 *
 * 版权所有 (C) 2026 黛 (Dye) & AEFR Contributors
 *
 * 本程序是自由软件：您可以自由分发和/或修改它。
 * 它遵循由自由软件基金会（Free Software Foundation）发布的
 * GNU 通用公共许可证（GNU General Public License）第 3 版。
 *本程序的 git 仓库应带有 GPL3 许可证，请自行查看
 */

// 全局禁用rust的大傻福警告
#![allow(warnings)]

// --- 核心依赖 ---
use eframe::egui;
use egui::{
    epaint::Vertex, Color32, FontData, FontDefinitions, FontFamily, Mesh, Pos2, Rect, Shape,
    TextureHandle, TextureId, Vec2,
};
use rayon::prelude::*;
use rusty_spine::{
    AnimationState, AnimationStateData, Atlas, Skeleton, SkeletonJson, Slot, Physics,
};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

// ============================================================================
// 1. 智能多平台入口
//    采用“无冲突”写法，确保桌面、Termux、原生APK都能独立编译通过。
// ============================================================================

/// 桌面端 (Windows, macOS, Linux) 入口
#[cfg(not(target_os = "android"))]
fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0])
            .with_title("AEFR - OxidizedSchale Edition"),
        ..Default::default()
    };
    eframe::run_native("AEFR_App", options, Box::new(|cc| Box::new(AefrApp::new(cc))))
}

/// Termux 命令行环境入口
#[cfg(target_os = "android")]
fn main() -> eframe::Result<()> {
    // Termux 环境下，我们依然创建一个标准的窗口
    let options = eframe::NativeOptions::default();
    eframe::run_native("AEFR_App", options, Box::new(|cc| Box::new(AefrApp::new(cc))))
}

/// 安卓原生 APK 入口 (由操作系统拉起)
#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: android_activity::AndroidApp) {
    // 对于 APK 打包，eframe 会自动处理 AndroidApp 上下文
    let options = eframe::NativeOptions::default();
    let _ = eframe::run_native("AEFR_App", options, Box::new(|cc| Box::new(AefrApp::new(cc))));
}


// ============================================================================
// 2. 异步指令系统
//    解耦UI线程和耗时操作（如文件IO、图片解析），保证界面始终流畅。
// ============================================================================

#[derive(Debug)]
enum AppCommand {
    /// 更新对话内容
    Dialogue { name: String, affiliation: String, content: String },
    /// 请求异步加载一个 Spine 角色
    RequestLoad { slot_idx: usize, path: String },
    /// 角色加载成功
    LoadSuccess(usize, Box<SpineObject>),
    /// 异步加载背景图片
    LoadBackground(String),
    /// 向控制台打印日志
    Log(String),
}


// ============================================================================
// 3. Spine 渲染核心
//    封装了 Spine 动画的加载、更新和绘制逻辑。
// ============================================================================

pub struct SpineObject {
    skeleton: Skeleton,
    state: AnimationState,
    _texture: TextureHandle,
    texture_id: TextureId,
    pub position: Pos2,
    pub scale: f32,
}

// 手动实现Debug，因为Spine原生对象不支持自动派生
impl std::fmt::Debug for SpineObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpineObject")
         .field("position", &self.position)
         .finish()
    }
}

// 标记为 `Send`，允许在线程间安全传递
unsafe impl Send for SpineObject {}

impl SpineObject {
    /// 异步加载 Spine 角色资源
    fn load_async(ctx: &egui::Context, path_str: &str) -> Option<Self> {
        let atlas_path = std::path::Path::new(path_str);
        let atlas = std::sync::Arc::new(Atlas::new_from_file(atlas_path).ok()?);
        
        let (texture_handle, texture_id) = if let Some(page) = atlas.pages().next() {
            let img_path = atlas_path.parent()?.join(page.name());
            let img = image::open(&img_path).ok()?;
            let size = [img.width() as usize, img.height() as usize];
            let rgba8 = img.to_rgba8();
            let c_img = egui::ColorImage::from_rgba_unmultiplied(size, rgba8.as_raw());
            
            // --- 修复 E0382: 所有权问题 ---
            // 我们必须先调用 .id() 获取ID，然后再将 h（TextureHandle）的所有权移出
            let h = ctx.load_texture(page.name(), c_img, egui::TextureOptions::LINEAR);
            let id = h.id();
            (h, id)

        } else { return None; };

        let json_path = atlas_path.with_extension("json");
        let skeleton_json = SkeletonJson::new(atlas);
        let skeleton_data = std::sync::Arc::new(skeleton_json.read_skeleton_data_file(json_path).ok()?);
        let state_data = std::sync::Arc::new(AnimationStateData::new(skeleton_data.clone()));
        let mut state = AnimationState::new(state_data);
        if let Some(anim) = skeleton_data.animations().next() { let _ = state.set_animation(0, &anim, true); }

        Some(Self {
            skeleton: Skeleton::new(skeleton_data),
            state,
            _texture: texture_handle,
            texture_id,
            position: Pos2::new(0.0, 0.0), // 初始位置，将在加载成功后设置
            scale: 0.5,
        })
    }
    
    /// 并行更新动画状态
    fn update_parallel(&mut self, dt: f32) {
        self.state.update(dt);
        let _ = self.state.apply(&mut self.skeleton);
        self.skeleton.update_world_transform(Physics::None);
    }

    /// 将 Spine 骨骼绘制到 egui 的 UI 上
    fn paint(&self, ui: &mut egui::Ui) {
        let mut mesh = Mesh::with_texture(self.texture_id);
        let mut world_vertices = Vec::with_capacity(1024);
        for slot in self.skeleton.draw_order() {
            if let Some(attachment) = slot.attachment() {
                if let Some(region) = attachment.as_region() {
                    unsafe {
                        if world_vertices.len() < 8 { world_vertices.resize(8, 0.0); }
                        region.compute_world_vertices(&*slot, &mut world_vertices, 0, 2);
                        self.push_to_mesh(&mut mesh, &world_vertices[0..8], &region.uvs(), &[0, 1, 2, 2, 3, 0], &*slot, region.color());
                    }
                } else if let Some(mesh_att) = attachment.as_mesh() {
                    unsafe {
                        let len = mesh_att.world_vertices_length() as usize;
                        if world_vertices.len() < len { world_vertices.resize(len, 0.0); }
                        mesh_att.compute_world_vertices(&*slot, 0, len as i32, &mut world_vertices, 0, 2);
                        let uvs = std::slice::from_raw_parts(mesh_att.uvs(), len);
                        let tris = std::slice::from_raw_parts(mesh_att.triangles(), mesh_att.triangles_count() as usize);
                        self.push_to_mesh(&mut mesh, &world_vertices[0..len], uvs, tris, &*slot, mesh_att.color());
                    }
                }
            }
        }
        ui.painter().add(Shape::mesh(mesh));
    }

    /// 辅助函数：将计算好的顶点数据推入 egui 的 Mesh
    fn push_to_mesh(&self, mesh: &mut Mesh, w_v: &[f32], uvs: &[f32], tris: &[u16], slot: &Slot, att_c: rusty_spine::Color) {
        let s_c = slot.color();
        let color = Color32::from_rgba_premultiplied(
            (s_c.r * att_c.r * 255.0) as u8, (s_c.g * att_c.g * 255.0) as u8,
            (s_c.b * att_c.b * 255.0) as u8, (s_c.a * att_c.a * 255.0) as u8,
        );
        let idx_offset = mesh.vertices.len() as u32;
        let count = usize::min(uvs.len() / 2, w_v.len() / 2);
        for i in 0..count {
            let pos = Pos2::new(w_v[i*2] * self.scale + self.position.x, -w_v[i*2+1] * self.scale + self.position.y);
            mesh.vertices.push(Vertex { pos, uv: Pos2::new(uvs[i*2], uvs[i*2+1]), color });
        }
        for &idx in tris { mesh.indices.push(idx_offset + idx as u32); }
    }
}


// ============================================================================
// 4. 应用主逻辑 (AefrApp)
//    包含所有状态管理、UI渲染循环和事件处理。
// ============================================================================

struct AefrApp {
    // --- 对话 & 剧情 ---
    current_name: String,
    current_affiliation: String,
    
    // --- 打字机效果核心 ---
    target_chars: Vec<char>,
    visible_count: usize,
    type_timer: f32,
    type_speed: f32,

    // --- 场景资源 ---
    characters: Vec<Option<SpineObject>>,
    background: Option<TextureHandle>,

    // --- 异步通信 ---
    tx: Sender<AppCommand>,
    rx: Receiver<AppCommand>,

    // --- 交互式控制台 ---
    console_open: bool,
    console_input: String,
    console_logs: Vec<String>,
}

impl AefrApp {
    /// 应用初始化
    fn new(cc: &eframe::CreationContext) -> Self {
        setup_custom_fonts(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let (tx, rx) = channel();
        
        Self {
            current_name: "System".into(),
            current_affiliation: "AEFR".into(),
            target_chars: "Welcome to AEFR (AEFR's Eternal Freedom & Rust-rendered)!".chars().collect(),
            visible_count: 0, 
            type_timer: 0.0,
            type_speed: 0.03,
            characters: (0..5).map(|_| None).collect(),
            background: None,
            tx, rx,
            console_open: false,
            console_input: String::new(),
            console_logs: vec!["AEFR Console Ready. Type 'HELP' for commands.".into()],
        }
    }

    /// 解析控制台指令并发送异步命令
    fn parse_and_send_command(&mut self) {
        let input = self.console_input.trim().to_owned();
        if input.is_empty() { return; }
        self.console_logs.push(format!("> {}", input));

        let tx = self.tx.clone();
        if let Some(rest) = input.strip_prefix("LOAD ") {
            let parts: Vec<&str> = rest.splitn(2, ' ').collect();
            if parts.len() == 2 {
                if let Ok(idx) = parts[0].parse::<usize>() {
                    tx.send(AppCommand::RequestLoad { slot_idx: idx, path: parts[1].replace("\"", "") }).ok();
                }
            }
        } else if let Some(rest) = input.strip_prefix("TALK ") {
            let p: Vec<&str> = rest.split('|').collect();
            if p.len() == 3 {
                tx.send(AppCommand::Dialogue { name: p[0].to_owned(), affiliation: p[1].to_owned(), content: p[2].to_owned() }).ok();
            }
        } else if let Some(path) = input.strip_prefix("BG ") {
            tx.send(AppCommand::LoadBackground(path.replace("\"", ""))).ok();
        } else if input.eq_ignore_ascii_case("HELP") {
            self.console_logs.push("Commands:".into());
            self.console_logs.push("  LOAD <0-4> \"<path_to_atlas>\"".into());
            self.console_logs.push("  TALK \"<name>|<affiliation>|<message>\"".into());
            self.console_logs.push("  BG \"<path_to_image>\"".into());
        }
        self.console_input.clear();
    }

    /// 处理从子线程返回的异步事件
    fn handle_async_events(&mut self, ctx: &egui::Context) {
        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                AppCommand::Dialogue { name, affiliation, content } => { 
                    self.current_name = name; 
                    self.current_affiliation = affiliation; 
                    self.target_chars = content.chars().collect();
                    self.visible_count = 0;
                    self.type_timer = 0.0;
                }
                AppCommand::Log(msg) => self.console_logs.push(msg),
                AppCommand::RequestLoad { slot_idx, path } => {
                    let tx_cb = self.tx.clone();
                    let ctx_clone = ctx.clone();
                    self.console_logs.push(format!("Requesting load for slot {}...", slot_idx));
                    // 异步加载：将耗时的IO和解析操作扔到子线程，避免UI卡顿
                    thread::spawn(move || {
                        if let Some(obj) = SpineObject::load_async(&ctx_clone, &path) {
                            tx_cb.send(AppCommand::LoadSuccess(slot_idx, Box::new(obj))).ok();
                        } else {
                            tx_cb.send(AppCommand::Log(format!("Load failed: {}", path))).ok();
                        }
                    });
                }
                AppCommand::LoadSuccess(idx, obj) => {
                    if let Some(slot) = self.characters.get_mut(idx) {
                        let mut loaded = *obj;
                        // 根据槽位设置默认位置
                        loaded.position = Pos2::new(200.0 + idx as f32 * 220.0, 720.0);
                        *slot = Some(loaded);
                        self.console_logs.push(format!("Slot {} loaded successfully.", idx));
                    }
                }
                AppCommand::LoadBackground(path) => {
                    if let Ok(img) = image::open(&path) {
                        let rgba = img.to_rgba8();
                        let c_img = egui::ColorImage::from_rgba_unmultiplied([img.width() as _, img.height() as _], rgba.as_raw());
                        self.background = Some(ctx.load_texture(&path, c_img, egui::TextureOptions::LINEAR));
                        self.console_logs.push("Background loaded.".into());
                    } else {
                        self.console_logs.push(format!("Failed to load background: {}", path));
                    }
                }
            }
        }
    }
}

impl eframe::App for AefrApp {
    /// 主更新循环，每帧调用
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 1. 处理异步返回的事件
        self.handle_async_events(ctx);
        let dt = ctx.input(|i| i.stable_dt);

        // 2. 更新打字机逻辑
        if self.visible_count < self.target_chars.len() {
            self.type_timer += dt;
            if self.type_timer > self.type_speed {
                self.visible_count += 1;
                self.type_timer = 0.0;
                ctx.request_repaint(); // 请求重绘以产生动画
            }
        }

        // 3. 并行更新所有 Spine 角色的动画
        //    利用 Rayon，让所有角色的动画在多核CPU上同时计算，提高性能
        self.characters.par_iter_mut().for_each(|slot| {
            if let Some(char) = slot { char.update_parallel(dt); }
        });
        if self.characters.iter().any(|c| c.is_some()) { ctx.request_repaint(); }

        // 4. 绘制所有 UI 元素
        egui::CentralPanel::default().show(ctx, |ui| {
            let screen_rect = ui.max_rect();
            
            // 绘制背景
            if let Some(bg) = &self.background {
                ui.painter().image(bg.id(), screen_rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            }

            // 绘制角色
            for char in self.characters.iter().flatten() { char.paint(ui); }

            // 绘制对话框
            let current_text_slice: String = self.target_chars.iter().take(self.visible_count).collect();
            let clicked = draw_dialogue_ui(ui, screen_rect, &self.current_name, &self.current_affiliation, &current_text_slice);
            
            // 如果点击对话框，则瞬间显示所有文字
            if clicked && self.visible_count < self.target_chars.len() {
                self.visible_count = self.target_chars.len();
            }

            // 绘制控制台 UI
            let cmd_rect = Rect::from_min_size(Pos2::new(10.0, 10.0), Vec2::new(60.0, 40.0));
            if ui.put(cmd_rect, egui::Button::new("CMD")).clicked() { self.console_open = !self.console_open; }
            if self.console_open {
                draw_console_window(ctx, self);
            }
        });
    }
}


// ============================================================================
// 5. UI 绘制函数
// ============================================================================

/// 绘制对话框 UI，并返回是否被点击
fn draw_dialogue_ui(ui: &mut egui::Ui, screen: Rect, name: &str, affiliation: &str, content: &str) -> bool {
    let box_h = 160.0;
    let box_rect = Rect::from_min_max(Pos2::new(0.0, screen.bottom() - box_h), screen.max);
    
    // 绘制半透明背景
    ui.painter().rect_filled(box_rect, 5.0, Color32::from_black_alpha(180));
    
    // 技巧：放置一个透明的按钮来捕获整个对话框的点击事件
    let response = ui.allocate_rect(box_rect, egui::Sense::click());
    
    // 绘制名字和所属
    if !name.is_empty() {
        let name_pos = box_rect.left_top() + Vec2::new(100.0, 20.0);
        ui.painter().text(
            name_pos, egui::Align2::LEFT_TOP,
            format!("{} [{}]", name, affiliation),
            egui::FontId::proportional(22.0),
            Color32::WHITE
        );
    }
    
    // 绘制对话内容
    ui.painter().text(
        box_rect.left_top() + Vec2::new(100.0, 50.0),
        egui::Align2::LEFT_TOP,
        content,
        egui::FontId::proportional(26.0),
        Color32::WHITE
    );

    response.clicked()
}

/// 绘制交互式控制台窗口
fn draw_console_window(ctx: &egui::Context, app: &mut AefrApp) {
    egui::Window::new("AEFR CONSOLE").default_size([600.0, 400.0]).show(ctx, |ui| {
        // 日志显示区域
        egui::ScrollArea::vertical().stick_to_bottom(true).max_height(300.0).show(ui, |ui| {
            for log in &app.console_logs { ui.monospace(log); }
        });
        ui.separator();
        
        // 指令输入区域
        ui.horizontal(|ui| {
            ui.monospace(" > ");
            let response = ui.text_edit_singleline(&mut app.console_input);
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                app.parse_and_send_command();
                response.request_focus(); // 保持焦点以便连续输入
            }
        });
    });
}


// ============================================================================
// 6. 工具函数
// ============================================================================

/// 设置自定义字体，以支持中文显示
fn setup_custom_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    // 优先使用安卓系统字体，其次是 Windows 字体
    let paths = vec!["/system/fonts/NotoSansCJK-Regular.ttc", "C:\\Windows\\Fonts\\msyh.ttc"];
    for p in paths {
        if let Ok(data) = std::fs::read(p) {
            fonts.font_data.insert("custom_font".into(), FontData::from_owned(data));
            // 将我们的自定义字体设为默认 proportional 字体
            if let Some(family) = fonts.families.get_mut(&FontFamily::Proportional) {
                family.insert(0, "custom_font".into());
            }
            ctx.set_fonts(fonts);
            return; // 找到一个就返回
        }
    }
                    }
