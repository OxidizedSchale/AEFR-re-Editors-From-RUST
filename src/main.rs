/*
 * Project: AEFR (AEFR's Eternal Freedom & Rust-rendered)
 * GitHub: https://github.com/OxidizedSchale/AEFR-s-Eternal-Freedom-Rust-rendered
 *
 * 版权所有 (C) 2026 OxidizedSchale & AEFR Contributors
 *
 * 本程序是自由软件：您可以自由分发和/或修改它。
 * 它遵循由自由软件基金会（Free Software Foundation）发布的
 * GNU 通用公共许可证（GNU General Public License）第 3 版。
 *本程序的 git 仓库应带有 GPL3 许可证，请自行查看
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

// 全局禁用 rust 的大傻逼警告
#![allow(warnings)]

use eframe::egui;
use egui::{
    epaint::Vertex, Color32, FontData, FontDefinitions, FontFamily, Mesh, Pos2, Rect, Shape,
    TextureHandle, TextureId, Vec2, Stroke,
};
use rayon::prelude::*; // 并行计算库
use rusty_spine::{
    AnimationState, AnimationStateData, Atlas, Skeleton, SkeletonJson, Slot, Physics,
};
use std::sync::mpsc::{channel, Receiver, Sender}; // 多线程通信通道
use std::thread;
use std::io::Cursor;
use std::sync::Arc; // 原子引用计数，用于线程间共享数据
use rodio::Source;

// ============================================================================
// 主函数入口与跨平台适配
// ============================================================================

// 非 Android 平台的主入口
#[cfg(not(target_os = "android"))]
fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 720.0]) // 设置窗口初始大小
            .with_title("AEFR - OxidizedSchale Edition"),
        vsync: true, // 开启垂直同步
        ..Default::default()
    };
    // 运行 eframe 应用
    eframe::run_native("AEFR_App", options, Box::new(|cc| Box::new(AefrApp::new(cc))))
}

// Android 平台的主入口
#[cfg(target_os = "android")]
fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions::default();
    eframe::run_native("AEFR_App", options, Box::new(|cc| Box::new(AefrApp::new(cc))))
}

// Android Activity 入口点（供 NDK 调用）
#[cfg(target_os = "android")]
#[no_mangle]
fn android_main(app: android_activity::AndroidApp) {
    let options = eframe::NativeOptions::default();
    let _ = eframe::run_native("AEFR_App", options, Box::new(|cc| Box::new(AefrApp::new(cc))));
}

// ============================================================================
// 通信与调度
// ============================================================================

// 自定义线程池调度器，用于管理并行计算任务
struct AefrScheduler { pool: rayon::ThreadPool }
impl AefrScheduler {
    fn new() -> Self {
        // 获取逻辑核心数，并预留2个核心给UI和音频线程
        let logic_cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let worker_count = if logic_cores > 2 { logic_cores - 2 } else { 1 };
        Self { pool: rayon::ThreadPoolBuilder::new().num_threads(worker_count).build().unwrap() }
    }
    // 在线程池中运行并行任务
    fn run_parallel<OP>(&self, op: OP) where OP: FnOnce() + Send { self.pool.install(op); }
}

// 应用内部命令枚举，用于跨线程通信
enum AppCommand {
    Dialogue { name: String, affiliation: String, content: String }, // 显示对话
    RequestLoad { slot_idx: usize, path: String }, // 请求加载 Spine 资源
    LoadSuccess(usize, Box<SpineObject>, egui::ColorImage, String, Vec<String>), // 加载成功回调
    LoadBackground(String), // 请求加载背景图片
    LoadBackgroundSuccess(egui::ColorImage), // 背景加载成功回调
    PlayBgm(String), // 播放背景音乐
    PlaySe(String), // 播放音效
    AudioReady(Vec<u8>, bool), // 音频数据准备就绪 (数据, 是否为BGM)
    StopBgm, // 停止背景音乐
    SetAnimation { slot_idx: usize, anim_name: String, loop_anim: bool }, // 设置动画
    Log(String), // 日志消息
}

// ============================================================================
// 音频管理
// ============================================================================

// 音频管理器，封装 rodio 的音频流和音轨
struct AudioManager {
    _stream: rodio::OutputStream, // 保持音频流存活
    _stream_handle: rodio::OutputStreamHandle, // 音频流句柄
    bgm_sink: rodio::Sink, // 背景音乐音轨
    se_sink: rodio::Sink, // 音效音轨
}

impl AudioManager {
    // 尝试初始化音频系统
    fn new() -> Option<Self> {
        let (_stream, stream_handle) = rodio::OutputStream::try_default().ok()?;
        let bgm_sink = rodio::Sink::try_new(&stream_handle).ok()?;
        let se_sink = rodio::Sink::try_new(&stream_handle).ok()?;
        Some(Self { _stream, _stream_handle: stream_handle, bgm_sink, se_sink })
    }
    
    // 播放背景音乐（循环）
    fn play_bgm(&self, data: Vec<u8>) {
        let cursor = Cursor::new(data);
        if let Ok(source) = rodio::Decoder::new(cursor) {
            self.bgm_sink.stop(); // 停止当前BGM
            self.bgm_sink.append(source.repeat_infinite()); // 设置循环播放
            self.bgm_sink.play();
        }
    }

    // 播放音效（单次）
    fn play_se(&self, data: Vec<u8>) {
        let cursor = Cursor::new(data);
        if let Ok(source) = rodio::Decoder::new(cursor) {
            self.se_sink.append(source);
            self.se_sink.play();
        }
    }

    fn stop_bgm(&self) { self.bgm_sink.stop(); } // 停止背景音乐
}

// ============================================================================
// Spine 核心对象
// ============================================================================

// Spine 动画对象，包含骨骼、状态、纹理等信息
pub struct SpineObject {
    skeleton: Skeleton, // 骨骼数据
    state: AnimationState, // 动画状态机
    _texture: Option<TextureHandle>, // 纹理句柄（用于保持所有权）
    texture_id: Option<TextureId>,   // 纹理 ID（用于渲染）
    pub position: Pos2, // 在屏幕上的位置
    pub scale: f32, // 缩放比例
    skeleton_data: Arc<rusty_spine::SkeletonData>, // 共享的骨骼数据
}
// 标记为 Send，允许在线程间传递
unsafe impl Send for SpineObject {}

impl SpineObject {
    // 异步加载 Spine 资源（不涉及 GPU 纹理上传）
    fn load_async_no_gpu(path_str: &str) -> Result<(Self, egui::ColorImage, String, Vec<String>), String> {
        let atlas_path = std::path::Path::new(path_str);
        // 1. 解析 .atlas 文件
        let atlas = Arc::new(Atlas::new_from_file(atlas_path).map_err(|_| "Failed to parse .atlas file")?);
        
        // 2. 获取图集第一页（通常只有一页）并加载对应图片
        let page = atlas.pages().next().ok_or("Atlas has no pages")?;
        let page_name = page.name().to_string();
        let img_path = atlas_path.parent().unwrap().join(&page_name);
        
        let img = image::open(&img_path).map_err(|_| format!("Cannot find image: {}", page_name))?;
        let size = [img.width() as usize, img.height() as usize];
        let rgba8 = img.to_rgba8();
        // 将图片数据转换为 egui 可用的格式
        let color_image = egui::ColorImage::from_rgba_unmultiplied(size, rgba8.as_raw());

        // 3. 解析 .json 骨骼文件
        let json_path = atlas_path.with_extension("json");
        let skeleton_json = SkeletonJson::new(atlas.clone());
        
        // Dirty Upgrade Script: 尝试将 Spine 3.8.x 数据升级到 4.1.x 格式
        let mut skeleton_data_opt = None;
        if let Ok(json_str) = std::fs::read_to_string(&json_path) {
            let mut hacked_json = json_str.replace("\"spine\":\"3..", "\"spine\":\"4.1.");
            hacked_json = hacked_json.replace("\"spine\": \"3..", "\"spine\": \"4.1.");
            if let Ok(data) = skeleton_json.read_skeleton_data(hacked_json.as_bytes()) {
                skeleton_data_opt = Some(Arc::new(data));
            }
        }
        
        // 如果升级失败，尝试直接加载原文件
        let skeleton_data = match skeleton_data_opt {
            Some(data) => data,
            None => {
                Arc::new(skeleton_json.read_skeleton_data_file(&json_path).map_err(|e| {
                    format!("Spine Ver Error: {}", e)
                })?)
            }
        };

        // 4. 创建动画状态和数据
        let state_data = Arc::new(AnimationStateData::new(skeleton_data.clone()));
        let mut state = AnimationState::new(state_data);

        // 收集所有动画名称
        let anim_names: Vec<String> = skeleton_data.animations().map(|a| a.name().to_string()).collect();
        // 默认播放第一个动画
        if let Some(anim) = skeleton_data.animations().next() { 
            let _ = state.set_animation(0, &anim, true); 
        }

        // 5. 创建并返回 Spine 对象
        let obj = Self {
            skeleton: Skeleton::new(skeleton_data.clone()),
            state,
            _texture: None,
            texture_id: None,
            position: Pos2::new(0.0, 0.0), // 初始位置
            scale: 0.5, // 初始缩放
            skeleton_data,
        };
        Ok((obj, color_image, page_name, anim_names))
    }

    // 获取当前立绘所有的动画名称
    pub fn get_anim_names(&self) -> Vec<String> {
        self.skeleton_data.animations().map(|a| a.name().to_string()).collect()
    }

    // 通过名称设置当前播放的动画
    fn set_animation_by_name(&mut self, anim_name: &str, loop_anim: bool) -> bool {
        if let Some(anim) = self.skeleton_data.animations().find(|a| a.name() == anim_name) {
            let _ = self.state.set_animation(0, &anim, loop_anim);
            true
        } else { false } // 未找到动画
    }
    
    // 并行更新动画状态（在调度器线程池中调用）
    fn update_parallel(&mut self, dt: f32) {
        self.state.update(dt); // 更新动画时间
        let _ = self.state.apply(&mut self.skeleton); // 将状态应用到骨骼
        self.skeleton.update_world_transform(Physics::None); // 更新骨骼世界变换
    }

    // 将当前帧的 Spine 骨骼渲染到 egui Mesh
    fn paint(&self, ui: &mut egui::Ui) {
        if let Some(tex_id) = self.texture_id {
            let mut mesh = Mesh::with_texture(tex_id); // 创建带纹理的网格
            let mut world_vertices = Vec::with_capacity(1024); // 预分配顶点缓冲区
            
            // 遍历绘制顺序中的每个插槽（Slot）
            for slot in self.skeleton.draw_order() {
                if let Some(attachment) = slot.attachment() {
                    if let Some(region) = attachment.as_region() { // 处理区域附件（简单四边形）
                        unsafe {
                            // 确保顶点缓冲区足够大
                            if world_vertices.len() < 8 { world_vertices.resize(8, 0.0); }
                            // 计算附件在世界空间中的顶点坐标
                            region.compute_world_vertices(&*slot, &mut world_vertices, 0, 2);
                            // 将顶点和索引推入网格
                            self.push_to_mesh(&mut mesh, &world_vertices[0..8], &region.uvs(), &[0, 1, 2, 2, 3, 0], &*slot, region.color());
                        }
                    } else if let Some(mesh_att) = attachment.as_mesh() { // 处理网格附件（复杂多边形）
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
            // 将构建好的网格添加到 UI 绘制器中
            ui.painter().add(Shape::mesh(mesh));
        }
    }

    // 辅助函数：将顶点、UV、颜色等信息推入 egui Mesh
    fn push_to_mesh(&self, mesh: &mut Mesh, w_v: &[f32], uvs: &[f32], tris: &[u16], slot: &Slot, att_c: rusty_spine::Color) {
        let s_c = slot.color(); // 插槽颜色（用于 tint）
        // 计算最终顶点颜色（插槽颜色 * 附件颜色）
        let color = Color32::from_rgba_premultiplied(
            (s_c.r * att_c.r * 255.0) as u8, (s_c.g * att_c.g * 255.0) as u8,
            (s_c.b * att_c.b * 255.0) as u8, (s_c.a * att_c.a * 255.0) as u8,
        );
        let count = usize::min(uvs.len() / 2, w_v.len() / 2); // 顶点数量
        let idx_offset = mesh.vertices.len() as u32; // 当前网格的顶点起始索引
        
        // 添加顶点
        for i in 0..count {
            // 应用缩放和位移，Y轴取反（屏幕坐标系与 Spine 坐标系不同）
            let pos = Pos2::new(w_v[i*2] * self.scale + self.position.x, -w_v[i*2+1] * self.scale + self.position.y);
            mesh.vertices.push(Vertex { pos, uv: Pos2::new(uvs[i*2], uvs[i*2+1]), color });
        }
        // 添加三角形索引
        for &idx in tris { mesh.indices.push(idx_offset + idx as u32); }
    }
}

// ============================================================================
// 应用主逻辑
// ============================================================================

// 应用主状态结构体
struct AefrApp {
    // 调度与 UI 状态
    scheduler: AefrScheduler,
    is_auto_enabled: bool, // 自动播放模式
    show_dialogue: bool, // 是否显示对话框
    current_name: String, // 当前说话角色名
    current_affiliation: String, // 当前角色所属
    target_chars: Vec<char>, // 目标文本字符数组
    visible_count: usize, // 已显示的字符数（用于打字机效果）
    type_timer: f32, // 打字效果计时器
    
    // 创作者面板/控制台状态
    console_open: bool, // 控制台是否打开
    selected_slot: usize, // 当前选中的角色槽位 (0-4)
    input_name: String, // 对话名字输入框
    input_aff: String, // 对话所属输入框
    input_content: String, // 对话内容输入框
    console_input: String, // 控制台命令行输入
    console_logs: Vec<String>, // 控制台日志
    
    // 动作预览窗口状态
    show_anim_preview: bool, // 是否显示动作预览面板
    preview_anim_idx: usize, // 当前正在预览的动作索引

    // 资源管理
    characters: Vec<Option<SpineObject>>, // 5个角色槽位
    background: Option<TextureHandle>, // 背景纹理
    audio_manager: Option<AudioManager>, // 音频管理器
    tx: Sender<AppCommand>, // 命令发送通道
    rx: Receiver<AppCommand>, // 命令接收通道
}

impl AefrApp {
    // 应用初始化
    fn new(cc: &eframe::CreationContext) -> Self {
        setup_embedded_font(&cc.egui_ctx); // 设置嵌入字体
        egui_extras::install_image_loaders(&cc.egui_ctx); // 安装图片加载器
        let (tx, rx) = channel(); // 创建跨线程通信通道
        
        // 初始化音频管理器
        let audio_manager = match AudioManager::new() {
            Some(mgr) => Some(mgr),
            None => { println!("Audio init failed"); None }
        };

        Self {
            scheduler: AefrScheduler::new(),
            is_auto_enabled: false,
            show_dialogue: false,
            current_name: "".into(),
            current_affiliation: "".into(),
            target_chars: vec![],
            visible_count: 0, 
            type_timer: 0.0,
            
            console_open: false,
            selected_slot: 0,
            input_name: "OxidizedSchale".into(), // 默认名字
            input_aff: "AEFR Contributors".into(), // 默认所属
            input_content: "AEFR 已启动\n 正在等待指令".into(), // 默认对话
            console_input: String::new(), 
            console_logs: vec!["[系统] AEFR 终端已就绪。".into(), "等待指令...".into()],
            
            show_anim_preview: false, // 默认隐藏预览面板
            preview_anim_idx: 0,      // 默认动作索引
            
            characters: (0..5).map(|_| None).collect(), // 初始化5个空槽位
            background: None,
            audio_manager,
            tx, rx,
        }
    }

    // 解析并发送控制台命令
    fn parse_and_send_command(&mut self, input: &str) {
        let input = input.trim().to_owned();
        if input.is_empty() { return; }
        self.console_logs.push(format!("> {}", input)); // 回显命令

        let tx = self.tx.clone();
        let cmd_upper = input.to_uppercase(); // 转换为大写以进行不区分大小写的匹配
        
        // 解析 LOAD 命令: LOAD <槽位索引> <文件路径>
        if cmd_upper.starts_with("LOAD ") {
            let parts: Vec<&str> = input.splitn(2, ' ').collect();
            if parts.len() == 2 {
                if let Ok(idx) = parts[0][5..].trim().parse::<usize>() {
                   tx.send(AppCommand::RequestLoad { slot_idx: idx, path: parts[1].replace("\"", "") }).ok();
                }
            }
        } 
        // 解析 ANIM 命令: ANIM <槽位索引> <动画名称> [是否循环]
        else if cmd_upper.starts_with("ANIM ") {
            let parts: Vec<&str> = input.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(idx) = parts[1].parse::<usize>() {
                    let anim_name = parts[2].to_string();
                    let loop_anim = parts.get(3).map(|s| s.to_lowercase() == "true").unwrap_or(true);
                    tx.send(AppCommand::SetAnimation { slot_idx: idx, anim_name, loop_anim }).ok();
                }
            }
        } 
        // 解析 BGM 命令: BGM <音频文件路径>
        else if cmd_upper.starts_with("BGM ") {
             let path = input[4..].trim().replace("\"", "");
             tx.send(AppCommand::PlayBgm(path)).ok();
        } 
        // 解析 SE 命令: SE <音频文件路径>
        else if cmd_upper.starts_with("SE ") {
             let path = input[3..].trim().replace("\"", "");
             tx.send(AppCommand::PlaySe(path)).ok();
        } 
        // 解析 STOP 命令: STOP (停止 BGM)
        else if cmd_upper == "STOP" {
             tx.send(AppCommand::StopBgm).ok();
        } 
        // 解析 TALK 命令: TALK <名字>|<所属>|<内容>
        else if cmd_upper.starts_with("TALK ") {
            let rest = &input[5..];
            let p: Vec<&str> = rest.split('|').collect();
            if p.len() == 3 {
                tx.send(AppCommand::Dialogue { name: p[0].to_owned(), affiliation: p[1].to_owned(), content: p[2].to_owned() }).ok();
            }
        } 
        // 解析 BG 命令: BG <图片文件路径>
        else if cmd_upper.starts_with("BG ") {
            let path = input[3..].trim().replace("\"", "");
            tx.send(AppCommand::LoadBackground(path)).ok();
        } 
        // 帮助命令
        else if cmd_upper == "HELP" {
            self.console_logs.push("可用指令: LOAD, ANIM, BGM, SE, BG, TALK".into());
        }
    }

    // 处理异步事件（从其他线程接收到的命令）
    fn handle_async_events(&mut self, ctx: &egui::Context) {
        while let Ok(cmd) = self.rx.try_recv() { // 尝试接收所有待处理命令
            match cmd {
                AppCommand::Dialogue { name, affiliation, content } => { 
                    // 设置对话内容，并初始化打字机效果
                    self.current_name = name; 
                    self.current_affiliation = affiliation; 
                    self.target_chars = content.chars().collect();
                    self.visible_count = 0;
                    self.show_dialogue = true;
                }
                AppCommand::Log(msg) => self.console_logs.push(msg), // 添加日志
                AppCommand::RequestLoad { slot_idx, path } => {
                    // 在后台线程加载 Spine 资源
                    let tx_cb = self.tx.clone();
                    self.console_logs.push(format!("[忙碌] 正在解析 Spine: {}", path));
                    thread::spawn(move || {
                        match SpineObject::load_async_no_gpu(&path) {
                            Ok((obj, color_image, page_name, anims)) => {
                                // 加载成功，传回主线程
                                tx_cb.send(AppCommand::LoadSuccess(slot_idx, Box::new(obj), color_image, page_name, anims)).ok();
                            },
                            Err(e) => {
                                tx_cb.send(AppCommand::Log(format!("[错误] 载入失败: {}", e))).ok();
                            }
                        }
                    });
                }
                AppCommand::LoadSuccess(idx, obj, color_image, page_name, anims) => {
                    // 在主线程中完成纹理上传和对象设置
                    if let Some(slot) = self.characters.get_mut(idx) {
                        let mut loaded = *obj;
                        // 将图片数据上传到 GPU 纹理
                        let handle = ctx.load_texture(page_name, color_image, egui::TextureOptions::LINEAR);
                        loaded.texture_id = Some(handle.id());
                        loaded._texture = Some(handle); // 保持纹理所有权，防止被释放
                        // 根据槽位索引设置水平位置
                        let x = match idx { 0 => 640.0, 1 => 400.0, 2 => 200.0, 3 => 880.0, 4 => 1080.0, _ => 640.0 };
                        loaded.position = Pos2::new(x, 720.0); // 底部对齐
                        loaded.scale = 0.6; // 设置缩放
                        *slot = Some(loaded);
                        self.console_logs.push(format!("[成功] 槽位 {} 就绪。包含 {} 个动作。", idx, anims.len()));
                    }
                }
                AppCommand::LoadBackground(path) => {
                    // 在后台线程加载背景图片
                    let tx_cb = self.tx.clone();
                    self.console_logs.push("[忙碌] 正在读取背景...".into());
                    thread::spawn(move || {
                        if let Ok(img) = image::open(&path) {
                            let rgba = img.to_rgba8();
                            let c_img = egui::ColorImage::from_rgba_unmultiplied([img.width() as _, img.height() as _], rgba.as_raw());
                            tx_cb.send(AppCommand::LoadBackgroundSuccess(c_img)).ok();
                        } else {
                            tx_cb.send(AppCommand::Log("[错误] 图片文件损坏或不存在".into())).ok();
                        }
                    });
                }
                AppCommand::LoadBackgroundSuccess(c_img) => {
                    // 在主线程中设置背景纹理
                    self.background = Some(ctx.load_texture("bg", c_img, egui::TextureOptions::LINEAR));
                    self.console_logs.push("[成功] 背景已切换。".into());
                }
                AppCommand::SetAnimation { slot_idx, anim_name, loop_anim } => {
                     // 设置指定槽位角色的动画
                     if let Some(Some(char)) = self.characters.get_mut(slot_idx) {
                         if char.set_animation_by_name(&anim_name, loop_anim) {
                             self.console_logs.push(format!("[成功] 槽位 {} 正在播放 '{}'", slot_idx, anim_name));
                         } else {
                             self.console_logs.push(format!("[警告] 动作未找到: {}", anim_name));
                         }
                     }
                }
                AppCommand::PlayBgm(path) => {
                    // 在后台线程读取 BGM 文件
                    let tx_cb = self.tx.clone();
                    thread::spawn(move || {
                        if let Ok(data) = std::fs::read(&path) {
                            tx_cb.send(AppCommand::AudioReady(data, true)).ok();
                        } else {
                            tx_cb.send(AppCommand::Log("[错误] 音频文件读取失败".into())).ok();
                        }
                    });
                }
                AppCommand::PlaySe(path) => {
                    // 在后台线程读取音效文件
                    let tx_cb = self.tx.clone();
                    thread::spawn(move || {
                        if let Ok(data) = std::fs::read(&path) {
                            tx_cb.send(AppCommand::AudioReady(data, false)).ok();
                        } else {
                            tx_cb.send(AppCommand::Log("[错误] 音效文件读取失败".into())).ok();
                        }
                    });
                }
                AppCommand::AudioReady(data, is_bgm) => {
                    // 在主线程播放音频（音频设备操作必须在主线程）
                    if let Some(mgr) = &self.audio_manager {
                        if is_bgm { mgr.play_bgm(data); self.console_logs.push("[音频] BGM 循环播放中".into()); }
                        else { mgr.play_se(data); self.console_logs.push("[音频] 音效已触发".into()); }
                    }
                }
                AppCommand::StopBgm => { if let Some(mgr) = &self.audio_manager { mgr.stop_bgm(); } } // 停止 BGM
            }
        }
    }
}

// 实现 eframe::App trait，定义应用主循环
impl eframe::App for AefrApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 1. 处理异步事件（命令）
        self.handle_async_events(ctx);
        let dt = ctx.input(|i| i.stable_dt); // 获取帧间隔时间

        // 2. 更新打字机效果
        if self.show_dialogue && self.visible_count < self.target_chars.len() {
            self.type_timer += dt;
            if self.type_timer > 0.03 { // 每0.03秒显示一个字符
                self.visible_count += 1;
                self.type_timer = 0.0;
            }
        }

        // 3. 并行更新所有角色的动画
        self.scheduler.run_parallel(|| {
            self.characters.par_iter_mut().for_each(|slot| {
                if let Some(char) = slot { char.update_parallel(dt); }
            });
        });

        // 4. 绘制主界面
        egui::CentralPanel::default().show(ctx, |ui| {
            let screen_rect = ui.max_rect(); // 获取屏幕矩形
            
            // 绘制背景
            if let Some(bg) = &self.background {
                ui.painter().image(bg.id(), screen_rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
            } else {
                ui.painter().rect_filled(screen_rect, 0.0, Color32::BLACK); // 默认黑色背景
            }

            // 绘制所有角色
            for char in self.characters.iter().flatten() { char.paint(ui); }

            // 绘制右上角按钮（AUTO, MENU）
            draw_top_right_buttons(ui, screen_rect, &mut self.is_auto_enabled);
            
            // 绘制对话框
            if self.show_dialogue {
                let current_text: String = self.target_chars.iter().take(self.visible_count).collect();
                // 传入打字完成状态
                let is_finished = self.visible_count >= self.target_chars.len();
                // 如果点击对话框，立即完成打字
                if draw_ba_dialogue(ui, screen_rect, &self.current_name, &self.current_affiliation, &current_text, is_finished) {
                    self.visible_count = self.target_chars.len();
                }
            }

            // 绘制命令行按钮
            let cmd_rect = Rect::from_min_size(Pos2::new(10.0, 10.0), Vec2::new(60.0, 30.0));
            if ui.put(cmd_rect, egui::Button::new("CMD")).clicked() { self.console_open = !self.console_open; }
            
            // 绘制创作者面板（控制台）
            if self.console_open { draw_creator_panel(ctx, self); }
        });

        ctx.request_repaint(); // 请求下一帧重绘
    }
}

// ============================================================================
// UI 复刻层（模仿《蔚蓝档案》风格的UI组件）
// ============================================================================

// 绘制右上角的 AUTO 和 MENU 按钮
fn draw_top_right_buttons(ui: &mut egui::Ui, screen: Rect, is_auto: &mut bool) {
    let btn_w = 90.0; // 按钮宽度
    let btn_h = 32.0; // 按钮高度
    let margin = 20.0; // 边距
    
    // AUTO 按钮位置
    let auto_pos = Pos2::new(screen.right() - btn_w * 2.0 - margin - 10.0, margin);
    let auto_rect = Rect::from_min_size(auto_pos, Vec2::new(btn_w, btn_h));
    
    let auto_resp = ui.allocate_rect(auto_rect, egui::Sense::click());
    if auto_resp.clicked() { *is_auto = !*is_auto; } // 切换自动播放状态

    // 根据状态改变按钮颜色
    let auto_bg = if *is_auto { Color32::from_rgb(255, 215, 0) } else { Color32::WHITE };
    let auto_fg = Color32::from_rgb(20, 30, 50);

    ui.painter().rect_filled(auto_rect, 4.0, auto_bg); // 绘制圆角矩形背景
    ui.painter().text(auto_rect.center(), egui::Align2::CENTER_CENTER, "AUTO", egui::FontId::proportional(18.0), auto_fg);

    // MENU 按钮（仅绘制，功能未实现）
    let menu_pos = Pos2::new(screen.right() - btn_w - margin, margin);
    let menu_rect = Rect::from_min_size(menu_pos, Vec2::new(btn_w, btn_h));
    let _ = ui.allocate_rect(menu_rect, egui::Sense::click());
    
    ui.painter().rect_filled(menu_rect, 4.0, Color32::WHITE);
    ui.painter().text(menu_rect.center(), egui::Align2::CENTER_CENTER, "MENU", egui::FontId::proportional(18.0), auto_fg);
}

// 绘制《蔚蓝档案》风格的对话框
// 返回布尔值表示是否被点击（用于快速跳过打字效果）
fn draw_ba_dialogue(ui: &mut egui::Ui, screen: Rect, name: &str, affiliation: &str, content: &str, is_finished: bool) -> bool {
    let box_h = 180.0; // 对话框高度
    let box_rect = Rect::from_min_max(Pos2::new(0.0, screen.bottom() - box_h), screen.max);
    
    // 绘制半透明黑色背景
    ui.painter().rect_filled(box_rect, 0.0, Color32::from_black_alpha(200));
    let response = ui.allocate_rect(box_rect, egui::Sense::click()); // 分配点击区域
    
    let pad_x = 100.0; // 左右内边距
    
    // 【关键修复】固定线条位置：顶部往下 55px (让出足够的名字高度)
    let line_y = box_rect.top() + 55.0;
    ui.painter().line_segment(
        [Pos2::new(pad_x, line_y), Pos2::new(screen.right() - pad_x, line_y)],
        Stroke::new(1.5, Color32::from_rgb(100, 120, 150)) // 分隔线
    );

    // 绘制角色名
    if !name.is_empty() {
        // 名字位置上移，保证不压线
        let name_pos = box_rect.left_top() + Vec2::new(pad_x, 15.0);
        let name_gal = ui.painter().layout_no_wrap(name.to_string(), egui::FontId::proportional(28.0), Color32::WHITE);
        ui.painter().galley(name_pos, name_gal.clone(), Color32::WHITE);
        
        // 绘制所属（在名字右侧）
        if !affiliation.is_empty() {
            let aff_pos = name_pos + Vec2::new(name_gal.rect.width() + 15.0, 6.0);
            ui.painter().text(aff_pos, egui::Align2::LEFT_TOP, affiliation, egui::FontId::proportional(22.0), Color32::from_rgb(100, 200, 255));
        }
    }
    
    // 绘制对话内容
    ui.painter().text(box_rect.left_top() + Vec2::new(pad_x, 80.0), egui::Align2::LEFT_TOP, content, egui::FontId::proportional(24.0), Color32::WHITE);
    
    // 【关键修复】只有打字结束后才显示倒三角提示符
    if is_finished {
        let time = ui.input(|i| i.time);
        let offset = (time * 3.0).sin() * 3.0; // 简单的上下浮动效果
        let tri_center = Pos2::new(screen.right() - pad_x, screen.bottom() - 30.0 + offset as f32);
        let size = 8.0;
        // 绘制倒三角形
        ui.painter().add(Shape::convex_polygon(
            vec![
                tri_center + Vec2::new(-size, -size),
                tri_center + Vec2::new(size, -size),
                tri_center + Vec2::new(0.0, size),
            ],
            Color32::from_rgb(0, 180, 255), // 蓝色三角形
            Stroke::NONE,
        ));
    }

    response.clicked() // 返回是否被点击
}

// 绘制创作者面板/控制台窗口
fn draw_creator_panel(ctx: &egui::Context, app: &mut AefrApp) {
    let mut cmd_to_send = None; // 临时存储待发送的命令

    egui::Window::new("创作者面板 (AEFR)").default_size([450.0, 500.0]).show(ctx, |ui| {
        ui.heading("📂 资源与槽位");
        
        // 槽位选择
        ui.horizontal(|ui| {
            ui.label("当前槽位:");
            for i in 0..5 {
                if ui.radio_value(&mut app.selected_slot, i, format!("[{}]", i)).clicked() {
                    app.console_logs.push(format!("[系统] 切换到槽位 {}", i));
                    app.preview_anim_idx = 0; // 切换槽位时重置预览动作索引
                }
            }
        });

        // 文件加载按钮（桌面端）
        ui.horizontal(|ui| {
            #[cfg(not(target_os = "android"))]
            {
                if ui.button("📥 载入 Spine (到当前槽)").clicked() {
                    if let Some(path) = rfd::FileDialog::new().add_filter("Atlas", &["atlas"]).pick_file() {
                        cmd_to_send = Some(AppCommand::RequestLoad { slot_idx: app.selected_slot, path: path.display().to_string() });
                    }
                }
                if ui.button("🖼 载入背景").clicked() {
                    if let Some(path) = rfd::FileDialog::new().add_filter("Images", &["png", "jpg"]).pick_file() {
                        cmd_to_send = Some(AppCommand::LoadBackground(path.display().to_string()));
                    }
                }
            }
            #[cfg(target_os = "android")]
            { ui.label("📌 移动端: 请使用底部命令行载入文件。"); } // Android 提示

            // 动作预览按钮（全平台可见，摆在右侧）
            if ui.button("🏃 预览动作").clicked() {
                app.show_anim_preview = true;
            }
        });

        ui.separator();
        ui.heading("🎵 音频控制");
        ui.horizontal(|ui| {
            #[cfg(not(target_os = "android"))]
            {
                if ui.button("🎼 载入 BGM (循环)").clicked() {
                    if let Some(path) = rfd::FileDialog::new().add_filter("Audio", &["mp3", "wav", "ogg"]).pick_file() {
                        cmd_to_send = Some(AppCommand::PlayBgm(path.display().to_string()));
                    }
                }
                if ui.button("🔊 载入 音效SE (单次)").clicked() {
                    if let Some(path) = rfd::FileDialog::new().add_filter("Audio", &["mp3", "wav", "ogg"]).pick_file() {
                        cmd_to_send = Some(AppCommand::PlaySe(path.display().to_string()));
                    }
                }
                if ui.button("⏹ 停止 BGM").clicked() {
                    cmd_to_send = Some(AppCommand::StopBgm);
                }
            }
        });

        ui.separator();
        ui.heading("💬 剧情对话");
        // 对话输入表单
        ui.horizontal(|ui| {
            ui.label("名字:");
            ui.add(egui::TextEdit::singleline(&mut app.input_name).desired_width(80.0));
            ui.label("所属:");
            ui.add(egui::TextEdit::singleline(&mut app.input_aff).desired_width(80.0));
        });
        ui.label("内容:");
        ui.add(egui::TextEdit::multiline(&mut app.input_content).desired_width(f32::INFINITY));
        
        if ui.button("▶ 发送对话 (TALK)").clicked() {
            cmd_to_send = Some(AppCommand::Dialogue {
                name: app.input_name.clone(),
                affiliation: app.input_aff.clone(),
                content: app.input_content.clone(),
            });
        }

        ui.separator();
        ui.heading("⌨️ 控制台输入");
        ui.horizontal(|ui| {
            let response = ui.add(egui::TextEdit::singleline(&mut app.console_input).hint_text("输入 LOAD, BG, ANIM 指令..."));
            // 点击发送按钮或按回车键发送命令
            if ui.button("发送指令").clicked() || (response.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter))) {
                let input = app.console_input.clone();
                app.parse_and_send_command(&input);
                app.console_input.clear();
                response.request_focus(); // 保持输入框焦点
            }
        });

        ui.separator();
        ui.heading("📜 系统日志");
        // 日志显示区域（自动滚动到底部）
        egui::ScrollArea::vertical().stick_to_bottom(true).max_height(100.0).show(ui, |ui| {
            for log in &app.console_logs { ui.label(log); }
        });
    });

    // ================= 新增：动作预览扩展窗口 =================
    if app.show_anim_preview {
        egui::Window::new("动作预览与选择")
            .collapsible(false)
            .resizable(false)
            .open(&mut app.show_anim_preview) // 提供自带的关闭 "X" 按钮
            .show(ctx, |ui| {
                if let Some(Some(char)) = app.characters.get(app.selected_slot) {
                    let anims = char.get_anim_names();
                    if anims.is_empty() {
                        ui.label("⚠️ 该立绘没有可用动作或解析失败。");
                    } else {
                        // 防止索引越界
                        if app.preview_anim_idx >= anims.len() {
                            app.preview_anim_idx = 0;
                        }
                        let current_anim = &anims[app.preview_anim_idx];

                        ui.vertical_centered(|ui| {
                            ui.label(format!("当前槽位 [{}] 动作:", app.selected_slot));
                            ui.heading(current_anim); // 大字显示当前动作名字
                            ui.add_space(10.0);

                            ui.horizontal(|ui| {
                                // 左箭头按钮
                                if ui.button("⬅ 上一个 (Prev)").clicked() {
                                    app.preview_anim_idx = (app.preview_anim_idx + anims.len() - 1) % anims.len();
                                    cmd_to_send = Some(AppCommand::SetAnimation {
                                        slot_idx: app.selected_slot,
                                        anim_name: anims[app.preview_anim_idx].clone(),
                                        loop_anim: true,
                                    });
                                }
                                
                                // 右箭头按钮
                                if ui.button("下一个 (Next) ➡").clicked() {
                                    app.preview_anim_idx = (app.preview_anim_idx + 1) % anims.len();
                                    cmd_to_send = Some(AppCommand::SetAnimation {
                                        slot_idx: app.selected_slot,
                                        anim_name: anims[app.preview_anim_idx].clone(),
                                        loop_anim: true,
                                    });
                                }
                            });
                        });
                    }
                } else {
                    ui.label(format!("⚠️ 槽位 [{}] 目前为空，请先载入立绘！", app.selected_slot));
                }
            });
    }

    // 在所有窗口布局完成后统一发送命令，避免借用冲突
    if let Some(cmd) = cmd_to_send {
        let _ = app.tx.send(cmd);
    }
}

// 设置嵌入字体（用于跨平台字体一致性）
fn setup_embedded_font(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let font_bytes = include_bytes!("font.ttf"); // 从二进制嵌入字体文件
    let font_data = FontData::from_static(font_bytes);
    fonts.font_data.insert("my_font".to_owned(), font_data);
    // 将自定义字体设为默认比例字体和等宽字体
    fonts.families.get_mut(&FontFamily::Proportional).unwrap().insert(0, "my_font".to_owned());
    fonts.families.get_mut(&FontFamily::Monospace).unwrap().insert(0, "my_font".to_owned());
    ctx.set_fonts(fonts);
}
