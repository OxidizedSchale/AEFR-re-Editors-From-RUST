/*
 * Project: AEFR (AEFR's Eternal Freedom & Rust-rendered)
 * GitHub: https://github.com/OxidizedSchale/AEFR-s-Eternal-Freedom-Rust-rendered
 *
 * 版权所有 (C) 2026 OxidizedSchale & AEFR Contributors
 *
 * 本程序是自由软件：您可以自由分发和/或修改它。
 * 它遵循由自由软件基金会（Free Software Foundation）发布的
 * GNU 通用公共许可证（GNU General Public License）第 3 版。
 * 本程序的 git 仓库应带有 GPL3 许可证，请自行查看
 *
 * ----------------------------------------------------------------------------
 *
 * [项目架构概述 / Architecture Overview]
 *
 * AEFR 是一个基于 Rust 的高性能《蔚蓝档案》二创编辑器引擎。
 * 它采用了以下核心技术栈：
 *
 * 1. UI 框架: egui (即时模式 GUI，极低内存占用) + eframe (跨平台后端)
 * 2. 渲染核心: rusty_spine (Spine 2D 运行时 C 绑定的 Rust 封装)
 * 3. 并行计算: rayon (用于多核 CPU 并行计算 5 人同屏的骨骼变形)
 * 4. 音频系统: rodio (异步音频流播放)
 * 5. 调度系统: 自研 "Gentleman Scheduler" (防止计算线程抢占 UI 和音频线程)
 *
 * [跨平台支持 / Cross-Platform]
 * - Windows / Linux / macOS (原生桌面应用)
 * - Android Termux (X11/Wayland 环境)
 * - Android APK (原生应用打包)
 *
 */
 
//全局禁用 Rust 的大傻逼警告
#![allow(warnings)]

// ============================================================================
// 依赖导入
// ============================================================================
use eframe::egui; // 主GUI框架
use egui::{
    epaint::Vertex, Color32, FontData, FontDefinitions, FontFamily, Mesh, Pos2, Rect, Shape,
    TextureHandle, TextureId, Vec2, Stroke,
};
use rayon::prelude::*; // 并行计算库
use rusty_spine::{
    AnimationState, AnimationStateData, Atlas, Skeleton, SkeletonJson, SkeletonBinary, Slot,
};
use std::sync::mpsc::{channel, Receiver, Sender}; // 线程间通信
use std::thread;
use std::io::Cursor;
use std::sync::Arc;
use rodio::Source; // 音频播放
use serde::{Serialize, Deserialize};

// ============================================================================
// 常量定义
// ============================================================================

const BASE_HEIGHT: f32 = 720.0; // 基础分辨率高度
const DIALOGUE_BOX_RATIO: f32 = 0.28; // 对话框占屏幕高度的比例
const MAX_DT: f32 = 0.033; // 最大帧时间（30FPS保护）

// ============================================================================
// 数据结构定义
// ============================================================================

/// 单幕场景数据
#[derive(Serialize, Deserialize, Clone, Default)]
struct Scene {
    bg_path: Option<String>,           // 背景图片路径
    bgm_path: Option<String>,          // 背景音乐路径
    char_paths: [Option<String>; 5],   // 5个角色槽位的Spine资源路径
    char_anims: [Option<String>; 5],   // 各角色当前动画
    speaker_name: String,              // 说话者姓名
    speaker_aff: String,               // 说话者所属（学校/组织）
    dialogue_content: String,          // 对话内容
}

/// 完整剧本（多幕场景集合）
#[derive(Serialize, Deserialize, Clone, Default)]
struct Scenario {
    scenes: Vec<Scene>,
}

// ============================================================================
// 程序入口点（跨平台处理）
// ============================================================================

#[cfg(not(target_os = "android"))]
fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0]) // 默认窗口大小
            .with_title("AEFR Editor - OxidizedSchale Edition"),
        vsync: true, // 开启垂直同步
        ..Default::default()
    };
    eframe::run_native("AEFR_App", options, Box::new(|cc| Box::new(AefrApp::new(cc))))
}

// Android平台特殊入口点
#[cfg(target_os = "android")]
fn main() -> eframe::Result<()> {
    eframe::run_native("AEFR_App", eframe::NativeOptions::default(), Box::new(|cc| Box::new(AefrApp::new(cc))))
}

// Android JNI入口（供原生Activity调用）
#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: android_activity::AndroidApp) {
    let _ = eframe::run_native("AEFR_App", eframe::NativeOptions::default(), Box::new(|cc| Box::new(AefrApp::new(cc))));
}

// ============================================================================
// 核心架构组件
// ============================================================================

/// 绅士调度器 - 防止计算线程抢占UI/音频线程
struct AefrScheduler { pool: rayon::ThreadPool }
impl AefrScheduler {
    fn new() -> Self {
        let logic_cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Self { 
            pool: rayon::ThreadPoolBuilder::new()
                .num_threads(if logic_cores > 2 { logic_cores - 2 } else { 1 }) // 保留2个核心给系统
                .build().unwrap() 
        }
    }
    fn run_parallel<OP>(&self, op: OP) where OP: FnOnce() + Send { 
        self.pool.install(op); // 在专用线程池中执行并行任务
    }
}

/// 应用命令枚举 - 线程间消息传递的指令
enum AppCommand {
    Dialogue { name: String, affiliation: String, content: String }, // 更新对话
    RequestLoad { slot_idx: usize, path: String }, // 异步加载角色
    LoadSuccess(usize, Box<SpineObject>, egui::ColorImage, String, Vec<String>), // 加载成功回调
    RemoveCharacter(usize), // 移除角色
    LoadBackground(String), // 加载背景
    LoadBackgroundSuccess(egui::ColorImage), // 背景加载成功
    PlayBgm(String), // 播放背景音乐
    PlaySe(String), // 播放音效
    AudioReady(Vec<u8>, bool), // 音频数据就绪
    StopBgm, // 停止背景音乐
    SetAnimation { slot_idx: usize, anim_name: String, loop_anim: bool }, // 设置动画
    Log(String), // 日志记录
}

/// 音频管理器
struct AudioManager {
    _stream: rodio::OutputStream,
    _stream_handle: rodio::OutputStreamHandle,
    bgm_sink: rodio::Sink, // BGM专用混音器
    se_sink: rodio::Sink,  // 音效专用混音器
}
impl AudioManager {
    fn new() -> Result<Self, String> {
        // 初始化音频输出流
        let (_stream, stream_handle) = rodio::OutputStream::try_default().map_err(|e| e.to_string())?;
        let bgm_sink = rodio::Sink::try_new(&stream_handle).map_err(|e| e.to_string())?;
        let se_sink = rodio::Sink::try_new(&stream_handle).map_err(|e| e.to_string())?;
        Ok(Self { _stream, _stream_handle: stream_handle, bgm_sink, se_sink })
    }
    
    fn play_bgm(&self, data: Vec<u8>) {
        // 解码并循环播放BGM
        if let Ok(source) = rodio::Decoder::new(Cursor::new(data)) {
            self.bgm_sink.stop(); 
            self.bgm_sink.append(source.repeat_infinite()); 
            self.bgm_sink.play();
        }
    }
    
    fn play_se(&self, data: Vec<u8>) {
        // 解码并播放音效（单次）
        if let Ok(source) = rodio::Decoder::new(Cursor::new(data)) { 
            self.se_sink.append(source); 
            self.se_sink.play(); 
        }
    }
    
    fn stop_bgm(&self) { self.bgm_sink.stop(); }
}

// ============================================================================
// Spine 2D骨骼动画对象
// ============================================================================

pub struct SpineObject {
    skeleton: Skeleton,               // Spine骨架实例
    state: AnimationState,            // 动画状态机
    _texture: Option<TextureHandle>,  // 纹理句柄（所有权管理）
    texture_id: Option<TextureId>,    // 纹理ID（渲染时使用）
    pub position: Pos2,               // 屏幕位置
    pub scale: f32,                   // 缩放比例
    skeleton_data: Arc<rusty_spine::SkeletonData>, // 共享骨架数据
}
unsafe impl Send for SpineObject {} // 标记为可跨线程安全发送

impl SpineObject {
    /// 异步加载Spine资源（不涉及GPU操作）
    fn load_async_no_gpu(path_str: &str) -> Result<(Self, egui::ColorImage, String, Vec<String>), String> {
        // 1. 加载.atlas文件
        let atlas_path = std::path::Path::new(path_str);
        let atlas = Arc::new(Atlas::new_from_file(atlas_path).map_err(|e| format!("Atlas Error: {}", e))?);
        let page = atlas.pages().next().ok_or("Atlas has no pages")?;
        let page_name = page.name().to_string();
        
        // 2. 加载纹理图片
        let img_path = atlas_path.parent().ok_or("Invalid path")?.join(&page_name);
        let img = image::open(&img_path).map_err(|e| format!("Image Load Error: {}", e))?;
        let color_image = egui::ColorImage::from_rgba_unmultiplied(
            [img.width() as _, img.height() as _], 
            img.to_rgba8().as_raw()
        );

        // 3. 加载骨骼数据（优先尝试二进制格式，其次JSON格式）
        let skel_path = atlas_path.with_extension("skel");
        let json_path = atlas_path.with_extension("json");
        
        let skeleton_data = if skel_path.exists() {
            let skeleton_bin = SkeletonBinary::new(atlas.clone());
            Arc::new(skeleton_bin.read_skeleton_data_file(&skel_path).map_err(|e| format!("Binary load failed: {}", e))?)
        } else if json_path.exists() {
            let skeleton_json = SkeletonJson::new(atlas.clone());
            Arc::new(skeleton_json.read_skeleton_data_file(&json_path).map_err(|e| format!("JSON load failed: {}", e))?)
        } else { 
            return Err("Missing .skel or .json".into()); 
        };

        // 4. 创建动画状态机和骨架实例
        let state_data = Arc::new(AnimationStateData::new(skeleton_data.clone()));
        let mut state = AnimationState::new(state_data);
        let anim_names: Vec<String> = skeleton_data.animations().map(|a| a.name().to_string()).collect();
        
        // 默认播放第一个动画
        if let Some(anim) = skeleton_data.animations().next() { 
            let _ = state.set_animation(0, &anim, true); 
        }

        Ok((Self { 
            skeleton: Skeleton::new(skeleton_data.clone()), 
            state, 
            _texture: None, 
            texture_id: None, 
            position: Pos2::ZERO, 
            scale: 0.45, 
            skeleton_data 
        }, color_image, page_name, anim_names))
    }

    /// 获取所有动画名称列表
    fn get_anim_names(&self) -> Vec<String> { 
        self.skeleton_data.animations().map(|a| a.name().to_string()).collect() 
    }
    
    /// 按名称设置动画
    fn set_animation_by_name(&mut self, anim_name: &str, loop_anim: bool) -> bool {
        if let Some(anim) = self.skeleton_data.animations().find(|a| a.name() == anim_name) {
            let _ = self.state.set_animation(0, &anim, loop_anim); 
            true
        } else { 
            false 
        }
    }
    
    /// 并行更新动画状态（由调度器调用）
    fn update_parallel(&mut self, dt: f32) {
        let dt = dt.min(MAX_DT); // 限制最大时间步长
        self.state.update(dt);
        self.skeleton.set_to_setup_pose(); // 重置到绑定姿势
        let _ = self.state.apply(&mut self.skeleton);
        self.skeleton.update_world_transform(); // 更新世界变换
        self.skeleton.update_cache(); // 更新缓存
    }
    
    /// 渲染Spine对象到egui Mesh
    fn paint(&self, ui: &mut egui::Ui) {
        let tex_id = match self.texture_id { 
            Some(id) => id, 
            None => return 
        };
        let mut mesh = Mesh::with_texture(tex_id);
        let mut world_vertices = Vec::with_capacity(1024); // 重用顶点缓冲区
        
        // 遍历所有绘制顺序的插槽
        for slot in self.skeleton.draw_order() {
            let attachment = match slot.attachment() { 
                Some(a) => a, 
                None => continue 
            };
            
            // 处理区域附件（RegionAttachment，普通图片）
            if let Some(region) = attachment.as_region() {
                unsafe {
                    if world_vertices.len() < 8 { world_vertices.resize(8, 0.0); }
                    region.compute_world_vertices(&slot.bone(), &mut world_vertices, 0, 2);
                    self.push_to_mesh(&mut mesh, &world_vertices[0..8], &region.uvs(), &[0, 1, 2, 2, 3, 0], &*slot, region.color());
                }
            } 
            // 处理网格附件（MeshAttachment，变形网格）
            else if let Some(mesh_att) = attachment.as_mesh() {
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
        ui.painter().add(Shape::mesh(mesh));
    }
    
    /// 将顶点数据推送到Mesh
    fn push_to_mesh(&self, mesh: &mut Mesh, w_v: &[f32], uvs: &[f32], tris: &[u16], slot: &Slot, att_c: rusty_spine::Color) {
        let s_c = slot.color(); // 插槽颜色（叠加附件颜色）
        let color = Color32::from_rgba_premultiplied(
            (s_c.r * att_c.r * 255.0) as u8, 
            (s_c.g * att_c.g * 255.0) as u8,
            (s_c.b * att_c.b * 255.0) as u8, 
            (s_c.a * att_c.a * 255.0) as u8,
        );
        
        // 添加顶点
        let count = usize::min(uvs.len() / 2, w_v.len() / 2);
        let idx_offset = mesh.vertices.len() as u32;
        
        for i in 0..count {
            let pos = Pos2::new(
                w_v[i*2] * self.scale + self.position.x, 
                -w_v[i*2+1] * self.scale + self.position.y // Y轴翻转（Spine使用向上为负的坐标系）
            );
            mesh.vertices.push(Vertex { 
                pos, 
                uv: Pos2::new(uvs[i*2], uvs[i*2+1]), 
                color 
            });
        }
        
        // 添加三角形索引
        for &idx in tris { 
            mesh.indices.push(idx_offset + idx as u32); 
        }
    }
}

// ============================================================================
// 主应用程序逻辑
// ============================================================================

struct AefrApp {
    scheduler: AefrScheduler,            // 绅士调度器
    is_auto_enabled: bool,               // 自动播放开关
    show_dialogue: bool,                 // 对话框显示开关
    scenario: Scenario,                   // 当前剧本
    current_scene_idx: usize,            // 当前场景索引
    target_chars: Vec<char>,             // 目标文本字符（打字机效果）
    visible_count: usize,                // 当前可见字符数
    type_timer: f32,                     // 打字机计时器
    console_open: bool,                  // 控制台面板开关
    selected_slot: usize,                // 当前选中的角色槽位
    console_input: String,               // 控制台输入缓冲
    console_logs: Vec<String>,           // 控制台日志
    show_anim_preview: bool,             // 动画预览面板开关
    preview_anim_idx: usize,             // 预览动画索引
    characters: Vec<Option<SpineObject>>, // 5个角色槽位
    background: Option<TextureHandle>,   // 背景纹理
    audio_manager: Option<AudioManager>, // 音频管理器（可选）
    tx: Sender<AppCommand>,              // 命令发送器
    rx: Receiver<AppCommand>,            // 命令接收器
}

impl AefrApp {
    fn new(cc: &eframe::CreationContext) -> Self {
        // 初始化配置
        setup_embedded_font(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let (tx, rx) = channel();
        let audio_manager = AudioManager::new().ok(); // 音频管理器可能初始化失败
        
        // 创建初始场景
        let startup_text = "AEFR 已启动\n正在等待指令......";
        let mut first_scene = Scene::default();
        first_scene.speaker_name = "OxidizedSchale".into();
        first_scene.speaker_aff = "AEFR Contributors".into();
        first_scene.dialogue_content = startup_text.into();

        Self {
            scheduler: AefrScheduler::new(),
            is_auto_enabled: true, 
            show_dialogue: true,
            scenario: Scenario { scenes: vec![first_scene] },
            current_scene_idx: 0,
            target_chars: startup_text.chars().collect(), 
            visible_count: 0, 
            type_timer: 0.0,
            console_open: false,
            selected_slot: 0,
            console_input: String::new(),
            console_logs: vec!["[系统] 编辑器就绪。".into()],
            show_anim_preview: false,
            preview_anim_idx: 0,
            characters: (0..5).map(|_| None).collect(), // 初始化5个空槽位
            background: None,
            audio_manager,
            tx, rx,
        }
    }

    /// 同步当前场景数据到UI状态
    fn sync_scene_to_ui(&mut self) {
        if let Some(scene) = self.scenario.scenes.get(self.current_scene_idx) {
            self.target_chars = scene.dialogue_content.chars().collect();
        }
    }

    /// 解析并发送控制台命令
    fn parse_and_send_command(&mut self, input: &str) {
        let input_trimmed = input.trim();
        if input_trimmed.is_empty() { return; }
        self.console_logs.push(format!("> {}", input_trimmed));
        let tx = self.tx.clone();
        let cmd_lower = input_trimmed.to_lowercase();

        // 命令解析逻辑
        if cmd_lower.starts_with("load ") { // load [槽位] [路径]
            let parts: Vec<&str> = input_trimmed.splitn(2, ' ').collect();
            if parts.len() == 2 {
                if let Ok(idx) = parts[0][5..].trim().parse::<usize>() {
                   let _ = tx.send(AppCommand::RequestLoad { 
                       slot_idx: idx, 
                       path: parts[1].replace("\"", "") 
                   });
                }
            }
        } else if cmd_lower.starts_with("anim ") { // anim [槽位] [动画名] [循环]
            let parts: Vec<&str> = input_trimmed.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(idx) = parts[1].parse::<usize>() {
                    let anim_name = parts[2].to_string();
                    let loop_anim = parts.get(3).map_or(true, |s| s.to_lowercase() == "true");
                    let _ = tx.send(AppCommand::SetAnimation { 
                        slot_idx: idx, 
                        anim_name, 
                        loop_anim 
                    });
                }
            }
        } else if cmd_lower.starts_with("bgm ") { // bgm [路径]
             let _ = tx.send(AppCommand::PlayBgm(input_trimmed[4..].trim().replace("\"", "")));
        } else if cmd_lower.starts_with("se ") { // se [路径]
             let _ = tx.send(AppCommand::PlaySe(input_trimmed[3..].trim().replace("\"", "")));
        } else if cmd_lower == "stop" { // stop
             let _ = tx.send(AppCommand::StopBgm);
        } else if cmd_lower.starts_with("talk ") { // talk [姓名]|[所属]|[内容]
            let p: Vec<&str> = input_trimmed[5..].split('|').collect();
            if p.len() == 3 {
                let _ = tx.send(AppCommand::Dialogue { 
                    name: p[0].into(), 
                    affiliation: p[1].into(), 
                    content: p[2].into() 
                });
            }
        } else if cmd_lower.starts_with("bg ") { // bg [路径]
            let _ = tx.send(AppCommand::LoadBackground(input_trimmed[3..].trim().replace("\"", "")));
        }
    }

    /// 处理异步事件（从其他线程接收）
    fn handle_async_events(&mut self, ctx: &egui::Context) {
        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                // 更新对话内容
                AppCommand::Dialogue { name, affiliation, content } => {
                    let scene = &mut self.scenario.scenes[self.current_scene_idx];
                    scene.speaker_name = name; 
                    scene.speaker_aff = affiliation; 
                    scene.dialogue_content = content;
                    self.sync_scene_to_ui(); 
                    self.visible_count = 0; // 触发打字机效果
                }
                // 日志记录
                AppCommand::Log(msg) => self.console_logs.push(msg),
                
                // 异步加载角色资源
                AppCommand::RequestLoad { slot_idx, path } => {
                    let tx_cb = self.tx.clone(); 
                    self.console_logs.push(format!("[解析] {}", path));
                    
                    let path_clone = path.clone();
                    thread::spawn(move || {
                        match SpineObject::load_async_no_gpu(&path_clone) {
                            Ok((obj, img, page, anims)) => { 
                                let _ = tx_cb.send(AppCommand::LoadSuccess(slot_idx, Box::new(obj), img, page, anims)); 
                            },
                            Err(e) => { 
                                let _ = tx_cb.send(AppCommand::Log(format!("[错误] {}", e))); 
                            }
                        }
                    });
                }
                
                // 角色加载成功回调
                AppCommand::LoadSuccess(idx, obj, color_image, page_name, anims) => {
                    if let Some(slot) = self.characters.get_mut(idx) {
                        let mut loaded = *obj;
                        // 加载纹理到GPU
                        let handle = ctx.load_texture(page_name, color_image, egui::TextureOptions::LINEAR);
                        loaded.texture_id = Some(handle.id()); 
                        loaded._texture = Some(handle);
                        *slot = Some(loaded);
                    }
                }
                
                // 移除角色
                AppCommand::RemoveCharacter(idx) => { 
                    self.characters[idx] = None; 
                }
                
                // 加载背景图片
                AppCommand::LoadBackground(path) => {
                    let tx_cb = self.tx.clone();
                    let path_clone = path.clone();
                    thread::spawn(move || {
                        if let Ok(img) = image::open(&path_clone) {
                            let c_img = egui::ColorImage::from_rgba_unmultiplied(
                                [img.width() as _, img.height() as _], 
                                img.to_rgba8().as_raw()
                            );
                            let _ = tx_cb.send(AppCommand::LoadBackgroundSuccess(c_img));
                        }
                    });
                    self.scenario.scenes[self.current_scene_idx].bg_path = Some(path);
                }
                
                // 背景加载成功回调
                AppCommand::LoadBackgroundSuccess(c_img) => {
                    self.background = Some(ctx.load_texture("bg", c_img, egui::TextureOptions::LINEAR));
                }
                
                // 设置动画
                AppCommand::SetAnimation { slot_idx, anim_name, loop_anim } => {
                     if let Some(Some(char)) = self.characters.get_mut(slot_idx) {
                         let _ = char.set_animation_by_name(&anim_name, loop_anim);
                     }
                }
                
                // 播放BGM
                AppCommand::PlayBgm(path) => {
                    let tx_cb = self.tx.clone();
                    let path_clone = path.clone();
                    thread::spawn(move || { 
                        if let Ok(d) = std::fs::read(&path_clone) { 
                            let _ = tx_cb.send(AppCommand::AudioReady(d, true)); 
                        } 
                    });
                    self.scenario.scenes[self.current_scene_idx].bgm_path = Some(path);
                }
                
                // 播放音效 (新增)
                AppCommand::PlaySe(path) => {
                    let tx_cb = self.tx.clone();
                    let path_clone = path.clone();
                    thread::spawn(move || { 
                        if let Ok(d) = std::fs::read(&path_clone) { 
                            // false 代表这是音效 (SE)，不循环
                            let _ = tx_cb.send(AppCommand::AudioReady(d, false)); 
                        } 
                    });
                }
                
                // 音频数据就绪
                AppCommand::AudioReady(data, is_bgm) => {
                    if let Some(mgr) = &self.audio_manager { 
                        if is_bgm { 
                            mgr.play_bgm(data); 
                        } else { 
                            mgr.play_se(data); 
                        } 
                    }
                }
                
                // 停止BGM
                AppCommand::StopBgm => { 
                    if let Some(mgr) = &self.audio_manager { 
                        mgr.stop_bgm(); 
                    } 
                }
                
                _ => {}
            }
        }
    }
}

// ============================================================================
// 主应用循环实现
// ============================================================================

impl eframe::App for AefrApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 处理异步事件
        self.handle_async_events(ctx);
        let dt = ctx.input(|i| i.stable_dt); // 获取稳定帧时间
        
        // 🌟 打字机效果更新
        if self.show_dialogue && self.visible_count < self.target_chars.len() {
            self.type_timer += dt;
            if self.type_timer > 0.03 { // 每0.03秒显示一个字符
                self.visible_count += 1; 
                self.type_timer = 0.0; 
            }
        }

        // 根据屏幕分辨率计算缩放比例
        let screen = ctx.screen_rect();
        let scale_factor = screen.height() / BASE_HEIGHT;
        
        // 更新角色位置（底部等距排列）
        for (i, slot) in self.characters.iter_mut().enumerate() {
            if let Some(char) = slot {
                char.scale = 0.45 * scale_factor;
                let x_percent = 0.15 + (i as f32 * 0.175); // 横向位置百分比
                char.position = Pos2::new(
                    screen.width() * x_percent, 
                    screen.bottom() + (30.0 * scale_factor) // 从屏幕底部开始
                );
            }
        }

        // 🌟 使用调度器并行更新所有角色的骨骼动画
        self.scheduler.run_parallel(|| {
            self.characters.par_iter_mut().for_each(|slot| {
                if let Some(char) = slot { 
                    char.update_parallel(dt); 
                }
            });
        });

        // 主绘制区域
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::BLACK))
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                
                // 绘制背景图片（如有）
                if let Some(bg) = &self.background {
                    let img_size = bg.size_vec2();
                    let scale = (rect.width() / img_size.x).max(rect.height() / img_size.y); // 保持比例缩放
                    ui.painter().image(
                        bg.id(), 
                        Rect::from_center_size(rect.center(), img_size * scale), 
                        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), 
                        Color32::WHITE
                    );
                }
                
                // 绘制所有角色
                for char in self.characters.iter().flatten() { 
                    char.paint(ui); 
                }
                
                // 右上角按钮
                draw_top_right_buttons(ui, rect, &mut self.is_auto_enabled);
                
                // 对话框
                if self.show_dialogue {
                    let scene = &self.scenario.scenes[self.current_scene_idx];
                    let text: String = self.target_chars.iter().take(self.visible_count).collect();
                    if draw_ba_dialogue(
                        ui, rect, 
                        &scene.speaker_name, 
                        &scene.speaker_aff, 
                        &text, 
                        self.visible_count >= self.target_chars.len()
                    ) { 
                        self.visible_count = self.target_chars.len(); // 点击跳过打字机
                    }
                }
                
                // 控制台开关按钮
                if ui.put(Rect::from_min_size(Pos2::new(10.0, 10.0), Vec2::new(60.0, 30.0)), 
                    egui::Button::new("CMD")).clicked() 
                { 
                    self.console_open = !self.console_open; 
                }
                
                // 控制台面板
                if self.console_open { 
                    draw_creator_panel(ctx, self); 
                }
            });
        
        ctx.request_repaint(); // 请求下一帧重绘
    }
}

// ============================================================================
// UI 组件函数
// ============================================================================

/// 绘制右上角按钮（AUTO/MENU）
fn draw_top_right_buttons(ui: &mut egui::Ui, screen: Rect, is_auto: &mut bool) {
    let (btn_w, btn_h, margin) = (90.0, 32.0, 20.0);
    
    // AUTO按钮
    let auto_rect = Rect::from_min_size(
        Pos2::new(screen.right() - btn_w * 2.0 - margin - 10.0, margin), 
        Vec2::new(btn_w, btn_h)
    );
    if ui.allocate_rect(auto_rect, egui::Sense::click()).clicked() { 
        *is_auto = !*is_auto; 
    }
    ui.painter().rect_filled(auto_rect, 4.0, 
        if *is_auto { Color32::from_rgb(255, 215, 0) } else { Color32::WHITE }
    );
    ui.painter().text(
        auto_rect.center(), 
        egui::Align2::CENTER_CENTER, 
        "AUTO", 
        egui::FontId::proportional(18.0), 
        Color32::from_rgb(20, 30, 50)
    );
    
    // MENU按钮
    ui.painter().rect_filled(
        Rect::from_min_size(Pos2::new(screen.right() - btn_w - margin, margin), Vec2::new(btn_w, btn_h)), 
        4.0, 
        Color32::WHITE
    );
    ui.painter().text(
        Pos2::new(screen.right() - btn_w / 2.0 - margin, margin + btn_h / 2.0), 
        egui::Align2::CENTER_CENTER, 
        "MENU", 
        egui::FontId::proportional(18.0), 
        Color32::from_rgb(20, 30, 50)
    );
}

/// 绘制蔚蓝档案风格对话框
/// 返回值：是否被点击（用于跳过打字机）
fn draw_ba_dialogue(ui: &mut egui::Ui, screen: Rect, name: &str, affiliation: &str, content: &str, is_finished: bool) -> bool {
    let box_h = screen.height() * DIALOGUE_BOX_RATIO; // 对话框高度
    let box_rect = Rect::from_min_max(Pos2::new(screen.left(), screen.bottom() - box_h), screen.max);
    let line_y = box_rect.top() + (box_h * 0.30); // 分割线Y位置
    
    // 1. 绘制暗蓝色背景 (高不透明度)
    let dark_blue_opaque = Color32::from_rgba_unmultiplied(12, 18, 28, 252);
    ui.painter().rect_filled(
        Rect::from_min_max(Pos2::new(screen.left(), line_y), screen.max), 
        0.0, 
        dark_blue_opaque
    );
    
    // 2. 🌟 严谨：使用 Mesh 绘制名字区域的垂直渐变淡出
    let gradient_rect = Rect::from_min_max(box_rect.left_top(), Pos2::new(screen.right(), line_y));
    let mut mesh = Mesh::default();
    // 使用 Unmultiplied 确保混合模式正确
    let color_bottom = Color32::from_rgba_unmultiplied(12, 18, 28, 245);
    let color_top = Color32::from_rgba_unmultiplied(12, 18, 28, 0);
    
    // 添加渐变四边形的四个顶点
    mesh.vertices.push(Vertex { pos: gradient_rect.left_top(), uv: Pos2::ZERO, color: color_top });
    mesh.vertices.push(Vertex { pos: gradient_rect.right_top(), uv: Pos2::ZERO, color: color_top });
    mesh.vertices.push(Vertex { pos: gradient_rect.right_bottom(), uv: Pos2::ZERO, color: color_bottom });
    mesh.vertices.push(Vertex { pos: gradient_rect.left_bottom(), uv: Pos2::ZERO, color: color_bottom });
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    ui.painter().add(Shape::mesh(mesh));

    // 对话框点击区域（用于跳过）
    let resp = ui.allocate_rect(box_rect, egui::Sense::click());
    
    // 绘制分割线
    let pad_x = (screen.width() * 0.08).max(100.0); // 两侧内边距
    ui.painter().line_segment(
        [Pos2::new(pad_x, line_y), Pos2::new(screen.right() - pad_x, line_y)], 
        Stroke::new(1.5, Color32::from_rgb(100, 120, 150))
    );

    // 绘制说话者姓名和所属
    if !name.is_empty() {
        let n_size = (box_h * 0.16).clamp(22.0, 30.0);
        let n_pos = box_rect.left_top() + Vec2::new(pad_x, box_h * 0.08);
        let n_gal = ui.painter().layout_no_wrap(name.into(), egui::FontId::proportional(n_size), Color32::WHITE);
        
        let n_width = n_gal.rect.width();
        let n_height = n_gal.rect.height();

        if !affiliation.is_empty() {
            let aff_size = n_size * 0.75;
            let aff_gal = ui.painter().layout_no_wrap(
                affiliation.into(), 
                egui::FontId::proportional(aff_size), 
                Color32::from_rgb(100, 200, 255)
            );
            let aff_height = aff_gal.rect.height();
            // 🌟 严谨：强制底部像素级对齐，多往上提 5px 抵消视觉误差
            let y_offset = n_height - aff_height - 5.0; 
            
            ui.painter().galley(n_pos, n_gal.clone(), Color32::WHITE);
            ui.painter().galley(
                n_pos + Vec2::new(n_width + 15.0, y_offset), 
                aff_gal, 
                Color32::from_rgb(100, 200, 255)
            );
        } else {
            ui.painter().galley(n_pos, n_gal, Color32::WHITE);
        }
    }
    
    // 🌟 严谨：内容上移，紧贴分割线
    ui.painter().text(
        Pos2::new(box_rect.left() + pad_x, line_y + box_h * 0.05), 
        egui::Align2::LEFT_TOP, 
        content, 
        egui::FontId::proportional((box_h * 0.13).clamp(18.0, 25.0)), 
        Color32::WHITE
    );
    
    // 绘制"继续"指示器（当文本显示完成时）
    if is_finished {
        let tri_center = Pos2::new(
            screen.right() - pad_x, 
            screen.bottom() - (box_h * 0.15) + (ui.input(|i| i.time) * 3.0).sin() as f32 * 3.0 // 上下浮动效果
        );
        let ts = box_h * 0.04; // 三角形大小
        ui.painter().add(Shape::convex_polygon(
            vec![
                tri_center + Vec2::new(-ts, -ts), 
                tri_center + Vec2::new(ts, -ts), 
                tri_center + Vec2::new(0.0, ts)
            ], 
            Color32::from_rgb(0, 180, 255), 
            Stroke::NONE
        ));
    }
    resp.clicked() // 返回是否被点击
}

/// 绘制创作者控制面板
fn draw_creator_panel(ctx: &egui::Context, app: &mut AefrApp) {
    let mut cmd_to_send = None; // 待发送的命令
    egui::Window::new("创作者面板 - AEFR v1.1")
        .default_size([500.0, 600.0])
        .show(ctx, |ui| {
            // 🎬 剧本幕数管理
            ui.heading("🎬 剧本幕数管理");
            ui.horizontal(|ui| {
                if ui.button("⬅ 上一幕").clicked() && app.current_scene_idx > 0 {
                    app.current_scene_idx -= 1; 
                    app.sync_scene_to_ui(); 
                    app.visible_count = app.target_chars.len(); // 立即显示全部文本
                }
                ui.label(format!(" 第 {} / {} 幕 ", app.current_scene_idx + 1, app.scenario.scenes.len()));
                if ui.button("下一幕 ➡").clicked() && app.current_scene_idx < app.scenario.scenes.len() - 1 {
                    app.current_scene_idx += 1; 
                    app.sync_scene_to_ui(); 
                    app.visible_count = app.target_chars.len();
                }
                ui.separator();
                if ui.button("➕ 增加一幕").clicked() {
                    let mut new_scene = app.scenario.scenes[app.current_scene_idx].clone();
                    new_scene.dialogue_content.clear();
                    app.scenario.scenes.insert(app.current_scene_idx + 1, new_scene);
                    app.current_scene_idx += 1; 
                    app.sync_scene_to_ui();
                }
                if ui.button("❌ 删除").clicked() && app.scenario.scenes.len() > 1 {
                    app.scenario.scenes.remove(app.current_scene_idx);
                    app.current_scene_idx = app.current_scene_idx.min(app.scenario.scenes.len() - 1);
                    app.sync_scene_to_ui();
                }
            });
            
            // 跳转输入
            ui.horizontal(|ui| {
                ui.label("跳转:");
                let mut jump = app.current_scene_idx + 1;
                let len = app.scenario.scenes.len();
                if ui.add(egui::DragValue::new(&mut jump).clamp_range(1..=len)).changed() {
                    app.current_scene_idx = jump - 1; 
                    app.sync_scene_to_ui(); 
                    app.visible_count = app.target_chars.len();
                }
            });

            ui.separator();
            
            // 保存/加载剧本
            ui.horizontal(|ui| {
                if ui.button("💾 保存剧本").clicked() {
                    if let Ok(json) = serde_json::to_string_pretty(&app.scenario) {
                        if let Some(p) = rfd::FileDialog::new()
                            .set_file_name("scenario.json")
                            .save_file() 
                        {
                            let _ = std::fs::write(p, json);
                        }
                    }
                }
                if ui.button("📂 重载剧本").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("JSON", &["json"])
                        .pick_file() 
                    {
                        if let Ok(data) = std::fs::read_to_string(p) {
                            if let Ok(s) = serde_json::from_str::<Scenario>(&data) {
                                app.scenario = s; 
                                app.current_scene_idx = 0; 
                                app.sync_scene_to_ui(); 
                                app.visible_count = app.target_chars.len();
                            }
                        }
                    }
                }
            });

            ui.separator();
            ui.heading("📂 资源管理");
            
// 槽位选择
            ui.horizontal(|ui| {
                ui.label("槽位:");
                for i in 0..5 { 
                    if ui.radio_value(&mut app.selected_slot, i, format!("[{}]", i)).clicked() { 
                        app.preview_anim_idx = 0; 
                    } 
                }
            });
            
            
            ui.horizontal(|ui| {
                if ui.button("📥 Spine立绘").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("Atlas", &["atlas"])
                        .pick_file() 
                    {
                        cmd_to_send = Some(AppCommand::RequestLoad { 
                            slot_idx: app.selected_slot, 
                            path: p.display().to_string() 
                        });
                    }
                }
                if ui.button("🖼 背景").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("Images", &["png", "jpg"])
                        .pick_file() 
                    {
                        cmd_to_send = Some(AppCommand::LoadBackground(p.display().to_string()));
                    }
                }
                if ui.add(egui::Button::new("移除立绘").fill(Color32::from_rgb(150, 40, 40))).clicked() {
                    cmd_to_send = Some(AppCommand::RemoveCharacter(app.selected_slot));
                }
                if ui.button("动作选择/预览").clicked() { 
                    app.show_anim_preview = true; 
                }
            });

            // --- 音频管理模块 ---
            ui.separator();
            ui.heading("🎵 音频管理");
            ui.horizontal(|ui| {
                // 1. 导入音乐（循环播放）
                if ui.button("🔁 导入音乐(循环)").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("Audio", &["mp3", "wav", "ogg"])
                        .pick_file() 
                    {
                        cmd_to_send = Some(AppCommand::PlayBgm(p.display().to_string()));
                    }
                }
                
                // 2. 音效（单次播放）
                if ui.button("🔊 音效").clicked() {
                    if let Some(p) = rfd::FileDialog::new()
                        .add_filter("Audio", &["mp3", "wav", "ogg"])
                        .pick_file() 
                    {
                        cmd_to_send = Some(AppCommand::PlaySe(p.display().to_string()));
                    }
                }
                
                // 3. 停止音乐
                if ui.add(egui::Button::new("⏹ 停止音乐").fill(Color32::from_rgb(150, 40, 40))).clicked() {
                    cmd_to_send = Some(AppCommand::StopBgm);
                }
            });

            ui.separator();
            ui.heading("💬 对话 (当前幕)");
            let scene = &mut app.scenario.scenes[app.current_scene_idx];
            
            // 说话者信息
            ui.horizontal(|ui| {
                ui.label("名称:"); 
                ui.add(egui::TextEdit::singleline(&mut scene.speaker_name).desired_width(80.0));
                ui.label("所属:"); 
                ui.add(egui::TextEdit::singleline(&mut scene.speaker_aff).desired_width(80.0));
            });
            
            // 🌟 TALK 按钮重新回归
            ui.add(egui::TextEdit::multiline(&mut scene.dialogue_content).desired_width(f32::INFINITY));
            if ui.button("▶ 发送对话 (TALK)").clicked() {
                app.sync_scene_to_ui();
                app.visible_count = 0; // 触发打字机
            }

            ui.separator();
            
            // 命令行输入
            ui.horizontal(|ui| {
                let res = ui.add(egui::TextEdit::singleline(&mut app.console_input).hint_text("CMD..."));
                if ui.button("发送").clicked() || (res.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter))) {
                    let input = app.console_input.clone(); 
                    app.parse_and_send_command(&input); 
                    app.console_input.clear(); 
                    res.request_focus();
                }
            });
            
            // 日志显示
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .max_height(60.0)
                .show(ui, |ui| { 
                    for log in &app.console_logs { 
                        ui.label(log); 
                    } 
                });
        });

    // 动画预览窗口
    if app.show_anim_preview {
        egui::Window::new("动作")
            .open(&mut app.show_anim_preview)
            .show(ctx, |ui| {
                 if let Some(Some(char)) = app.characters.get(app.selected_slot) {
                    let anims = char.get_anim_names();
                    if !anims.is_empty() {
                        if app.preview_anim_idx >= anims.len() { 
                            app.preview_anim_idx = 0; 
                        }
                        ui.heading(&anims[app.preview_anim_idx]);
                        ui.horizontal(|ui| {
                            if ui.button("⬅").clicked() { 
                                app.preview_anim_idx = (app.preview_anim_idx + anims.len() - 1) % anims.len(); 
                                cmd_to_send = Some(AppCommand::SetAnimation { 
                                    slot_idx: app.selected_slot, 
                                    anim_name: anims[app.preview_anim_idx].clone(), 
                                    loop_anim: true 
                                }); 
                            }
                            if ui.button("➡").clicked() { 
                                app.preview_anim_idx = (app.preview_anim_idx + 1) % anims.len(); 
                                cmd_to_send = Some(AppCommand::SetAnimation { 
                                    slot_idx: app.selected_slot, 
                                    anim_name: anims[app.preview_anim_idx].clone(), 
                                    loop_anim: true 
                                }); 
                            }
                        });
                    }
                 }
            });
    }
    
    // 发送待处理命令
    if let Some(cmd) = cmd_to_send { 
        let _ = app.tx.send(cmd); 
    }
}

/// 设置嵌入式字体（优化中文字体渲染）
fn setup_embedded_font(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let font_bytes = include_bytes!("font.ttf"); // 内嵌字体文件
    fonts.font_data.insert("my_font".to_owned(), FontData::from_static(font_bytes));
    fonts.families.get_mut(&FontFamily::Proportional).unwrap().insert(0, "my_font".to_owned());
    fonts.families.get_mut(&FontFamily::Monospace).unwrap().insert(0, "my_font".to_owned());
    ctx.set_fonts(fonts);
}
