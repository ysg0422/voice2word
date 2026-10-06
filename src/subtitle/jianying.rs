//! 剪映专业版（JianYing Pro）草稿生成器与本地工程自动注入器
//!
//! 支持生成微秒级精准对齐的 draft_content.json 与 draft_meta_info.json，
//! 并支持自动探测本机剪映安装路径，直接一键将工程注入剪映首页草稿列表。

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use super::segment::Segment;

pub struct JianYingExporter;

impl JianYingExporter {
    /// XML 特殊字符转义：字幕文本直接拼入 content 的内嵌 XML，
    /// 未转义的 & < > " 会使剪映解析草稿失败
    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    /// 自动探测本地剪映草稿工程根目录
    pub fn detect_local_draft_root() -> Option<PathBuf> {
        let local_app_data = std::env::var("LOCALAPPDATA").ok()?;
        let project_dir = PathBuf::from(&local_app_data)
            .join("JianyingPro")
            .join("User Data")
            .join("Projects")
            .join("com.lveditor.draft");

        let root_meta_path = project_dir.join("root_meta_info.json");
        if root_meta_path.exists() {
            if let Ok(content) = fs::read_to_string(&root_meta_path) {
                if let Ok(val) = serde_json::from_str::<Value>(&content) {
                    if let Some(store) = val.get("all_draft_store").and_then(|s| s.as_array()) {
                        if let Some(first) = store.first() {
                            if let Some(root) = first.get("draft_root_path").and_then(|r| r.as_str()) {
                                let p = PathBuf::from(root);
                                if p.exists() {
                                    return Some(p);
                                }
                            }
                        }
                    }
                }
            }
            return Some(project_dir);
        }

        // 尝试默认路径 D:\JianyingPro Drafts
        let d_drive = PathBuf::from(r"D:\JianyingPro Drafts");
        if d_drive.exists() {
            return Some(d_drive);
        }

        None
    }

    /// 获取剪映的 root_meta_info.json 路径
    pub fn get_root_meta_path() -> Option<PathBuf> {
        let local_app_data = std::env::var("LOCALAPPDATA").ok()?;
        let meta_path = PathBuf::from(&local_app_data)
            .join("JianyingPro")
            .join("User Data")
            .join("Projects")
            .join("com.lveditor.draft")
            .join("root_meta_info.json");
        if meta_path.exists() {
            Some(meta_path)
        } else {
            None
        }
    }

    /// 构造 draft_content.json 数据结构
    pub fn build_draft_content(
        segments: &[Segment],
        video_path: Option<&Path>,
        total_duration_sec: f64,
        bilingual: bool,
    ) -> Value {
        let duration_us = (total_duration_sec * 1_000_000.0) as u64;

        let mut texts = Vec::new();
        let mut text_segments = Vec::new();

        for (i, seg) in segments.iter().enumerate() {
            let text_id = Uuid::new_v4().to_string().to_uppercase();
            let seg_id = Uuid::new_v4().to_string().to_uppercase();
            let start_us = (seg.start * 1_000_000.0) as u64;
            let end_us = (seg.end * 1_000_000.0) as u64;
            let dur_us = end_us.saturating_sub(start_us).max(100_000);

            let raw_text = Self::xml_escape(&seg.project_export_text(bilingual));
            let content_xml = format!(
                r##"<font id="" path="" size="8.0"><color_val color="#ffffff">{}</color_val></font>"##,
                raw_text
            );

            // materials.texts item
            texts.push(json!({
                "add_type": 0,
                "alignment": 1,
                "background_alpha": 1.0,
                "background_color": "",
                "background_height": 0.14,
                "background_horizontal_offset": 0.0,
                "background_round_radius": 0.0,
                "background_style": 0,
                "background_vertical_offset": 0.0,
                "background_width": 0.14,
                "bold_width": 0.0,
                "border_alpha": 1.0,
                "border_color": "",
                "border_width": 0.08,
                "check_flag": 7,
                "content": content_xml,
                "font_category_id": "",
                "font_category_name": "",
                "font_id": "",
                "font_name": "",
                "font_path": "",
                "font_resource_id": "",
                "font_size": 8.0,
                "font_source_platform": 0,
                "font_team_id": "",
                "font_title": "none",
                "font_url": "",
                "fonts": [],
                "global_alpha": 1.0,
                "has_shadow": false,
                "id": text_id,
                "initial_scale": 1.0,
                "inner_padding": 0.0,
                "is_rich_text": false,
                "italic_degree": 0,
                "ktv_color": "",
                "layer_weight": 1,
                "letter_spacing": 0.0,
                "line_feed": 1,
                "line_max_width": 0.82,
                "line_spacing": 0.02,
                "name": format!("字幕片段_{}", i + 1),
                "original_size": [],
                "preset_category": "",
                "preset_category_id": "",
                "preset_has_set_alignment": false,
                "preset_id": "",
                "preset_index": 0,
                "preset_name": "",
                "shadow_alpha": 0.8,
                "shadow_angle": -45.0,
                "shadow_color": "#000000",
                "shadow_distance": 8.0,
                "shadow_point": { "x": 1.0, "y": -1.0 },
                "shadow_smoothing": 1.0,
                "shape_clip_x": false,
                "shape_clip_y": false,
                "style_name": "",
                "sub_type": 0,
                "text_alpha": 1.0,
                "text_color": "#ffffff",
                "text_size": 30,
                "text_to_audio_ids": [],
                "type": "subtitle",
                "typesetting": 0,
                "underline": false,
                "use_effect_default_color": false,
                "words": { "end_time": [], "start_time": [], "text": [] }
            }));

            // track segment item
            text_segments.push(json!({
                "caption_info": null,
                "cartoon": false,
                "clip": null,
                "common_keyframes": [],
                "enable_adjust": true,
                "enable_color_curves": true,
                "enable_color_match_adjust": false,
                "enable_color_wheels": true,
                "enable_lut": true,
                "enable_smart_color_adjust": false,
                "extra_material_refs": [],
                "group_id": "",
                "hdr_settings": null,
                "id": seg_id,
                "intensifies_audio_path": "",
                "is_placeholder": false,
                "is_tone_modify": false,
                "keyframe_refs": [],
                "last_nonzero_volume": 1.0,
                "material_id": text_id,
                "render_index": 14000 + i as u64,
                "responsive_layout": {
                    "enable": false,
                    "horizontal_pos_layout": 0,
                    "size_layout": 0,
                    "target_follow": "",
                    "vertical_pos_layout": 0
                },
                "reverse": false,
                "source_timerange": null,
                "speed": 1.0,
                "state": 0,
                "target_timerange": {
                    "duration": dur_us,
                    "start": start_us
                },
                "template_id": "",
                "track_attribute": 0,
                "track_render_index": 0,
                "visible": true,
                "volume": 1.0
            }));
        }

        let mut tracks = Vec::new();

        // 如果提供了主视频，加入主视频轨道
        let mut videos = Vec::new();
        let mut speeds = Vec::new();
        let mut canvases = Vec::new();

        if let Some(vpath) = video_path {
            let video_id = Uuid::new_v4().to_string().to_uppercase();
            let seg_id = Uuid::new_v4().to_string().to_uppercase();
            let speed_id = Uuid::new_v4().to_string().to_uppercase();
            let canvas_id = Uuid::new_v4().to_string().to_uppercase();

            let norm_vpath = vpath.to_string_lossy().replace('\\', "/");

            videos.push(json!({
                "aigc_type": "none",
                "audio_fade": null,
                "category_id": "",
                "category_name": "local",
                "check_flag": 63487,
                "crop": {
                    "lower_left_x": 0.0,
                    "lower_left_y": 1.0,
                    "lower_right_x": 1.0,
                    "lower_right_y": 1.0,
                    "upper_left_x": 0.0,
                    "upper_left_y": 0.0,
                    "upper_right_x": 1.0,
                    "upper_right_y": 0.0
                },
                "crop_ratio": "free",
                "crop_scale": 1.0,
                "duration": duration_us,
                "extra_type_option": 0,
                "formula_id": "",
                "freeze": null,
                "has_audio": true,
                "height": 1080,
                "id": video_id,
                "intensifies_audio_path": "",
                "intensifies_path": "",
                "is_ai_generating_voice": false,
                "is_unified_beauty_mode": false,
                "local_id": "",
                "local_material_id": "",
                "material_id": "",
                "material_name": vpath.file_name().and_then(|s| s.to_str()).unwrap_or("video"),
                "material_url": "",
                "matting": {
                    "flag": 0,
                    "has_handled": false,
                    "interactive_matting": null,
                    "matting_id": "",
                    "matting_rect": null,
                    "path": "",
                    "reverse": false,
                    "stroke_path": ""
                },
                "media_path": "",
                "object_locked": null,
                "origin_material_id": "",
                "path": norm_vpath,
                "reverse_intensifies_path": "",
                "reverse_path": "",
                "smart_crop": null,
                "smart_matting": null,
                "source": 0,
                "source_platform": 0,
                "stable": null,
                "team_id": "",
                "type": "video",
                "video_algorithm": {
                    "algorithms": [],
                    "deflicker": null,
                    "motion_blur_config": null,
                    "noise_reduction": null,
                    "path": "",
                    "quality_enhance": null,
                    "time_range": null
                },
                "width": 1920
            }));

            speeds.push(json!({
                "curve_speed": null,
                "id": speed_id,
                "mode": 0,
                "speed": 1.0,
                "type": "speed"
            }));

            canvases.push(json!({
                "album_image": "",
                "blur": 0.0,
                "color": "",
                "id": canvas_id,
                "image": "",
                "image_id": "",
                "image_name": "",
                "source_platform": 0,
                "team_id": "",
                "type": "canvas_color"
            }));

            tracks.push(json!({
                "attribute": 0,
                "flag": 0,
                "id": Uuid::new_v4().to_string().to_uppercase(),
                "is_default_name": true,
                "name": "视频轨道 1",
                "segments": [
                    {
                        "caption_info": null,
                        "cartoon": false,
                        "clip": {
                            "alpha": 1.0,
                            "flip": { "horizontal": false, "vertical": false },
                            "rotation": 0.0,
                            "scale": { "x": 1.0, "y": 1.0 },
                            "transform": { "x": 0.0, "y": 0.0 }
                        },
                        "common_keyframes": [],
                        "enable_adjust": true,
                        "enable_color_curves": true,
                        "enable_color_match_adjust": false,
                        "enable_color_wheels": true,
                        "enable_lut": true,
                        "enable_smart_color_adjust": false,
                        "extra_material_refs": [speed_id, canvas_id],
                        "group_id": "",
                        "hdr_settings": null,
                        "id": seg_id,
                        "intensifies_audio_path": "",
                        "is_placeholder": false,
                        "is_tone_modify": false,
                        "keyframe_refs": [],
                        "last_nonzero_volume": 1.0,
                        "material_id": video_id,
                        "render_index": 0,
                        "responsive_layout": {
                            "enable": false,
                            "horizontal_pos_layout": 0,
                            "size_layout": 0,
                            "target_follow": "",
                            "vertical_pos_layout": 0
                        },
                        "reverse": false,
                        "source_timerange": {
                            "duration": duration_us,
                            "start": 0
                        },
                        "speed": 1.0,
                        "state": 0,
                        "target_timerange": {
                            "duration": duration_us,
                            "start": 0
                        },
                        "template_id": "",
                        "track_attribute": 0,
                        "track_render_index": 0,
                        "visible": true,
                        "volume": 1.0
                    }
                ],
                "type": "video"
            }));
        }

        // 添加字幕轨道
        tracks.push(json!({
            "attribute": 0,
            "flag": 0,
            "id": Uuid::new_v4().to_string().to_uppercase(),
            "is_default_name": true,
            "name": "字幕轨道",
            "segments": text_segments,
            "type": "text"
        }));

        json!({
            "canvas_config": {
                "height": 1080,
                "ratio": "original",
                "width": 1920
            },
            "color_space": 0,
            "config": {
                "adjust_max_index": 1,
                "attachment_info": [],
                "combination_max_index": 1,
                "export_range": null,
                "extract_audio_last_index": 1,
                "lyrics_recognition_id": "",
                "lyrics_sync": true,
                "lyrics_taskinfo": [],
                "maintrack_adsorb": true,
                "material_save_mode": 0,
                "original_sound_last_index": 1,
                "record_audio_last_index": 1,
                "sticker_max_index": 1,
                "subtitle_recognition_id": "",
                "subtitle_sync": true,
                "subtitle_taskinfo": [],
                "system_font_list": [],
                "video_mute": false,
                "zoom_info_params": null
            },
            "cover": null,
            "create_time": 0,
            "duration": duration_us,
            "extra_info": null,
            "fps": 30.0,
            "free_render_index_mode_on": false,
            "group_container": null,
            "id": Uuid::new_v4().to_string().to_uppercase(),
            "keyframe_graph_list": [],
            "keyframes": {
                "adjusts": [],
                "audios": [],
                "effects": [],
                "filters": [],
                "handwrites": [],
                "stickers": [],
                "texts": [],
                "videos": []
            },
            "last_modified_platform": {
                "app_id": 3704,
                "app_source": "lv",
                "app_version": "5.9.0",
                "os": "windows"
            },
            "materials": {
                "audio_balances": [],
                "audio_effects": [],
                "audio_fades": [],
                "audio_track_indexes": [],
                "audios": [],
                "beats": [],
                "canvases": canvases,
                "chromas": [],
                "color_curves": [],
                "drafts": [],
                "effects": [],
                "flowers": [],
                "green_screens": [],
                "handwrites": [],
                "hsl": [],
                "images": [],
                "log_color_wheels": [],
                "loudnesses": [],
                "manual_deformations": [],
                "masks": [],
                "material_animations": [],
                "material_colors": [],
                "placeholders": [],
                "plugin_effects": [],
                "primary_color_wheels": [],
                "realtime_denoises": [],
                "shapes": [],
                "smart_crops": [],
                "sound_channel_mappings": [],
                "speeds": speeds,
                "stickers": [],
                "tail_leaders": [],
                "text_templates": [],
                "texts": texts,
                "transitions": [],
                "video_effects": [],
                "video_track_indexes": [],
                "videos": videos,
                "vocal_beautifys": [],
                "vocal_separations": []
            },
            "mutable_config": null,
            "name": "",
            "new_version": "100.0.0",
            "platform": {
                "app_id": 3704,
                "app_source": "lv",
                "app_version": "5.9.0",
                "os": "windows"
            },
            "relationships": [],
            "render_index_track_mode_on": false,
            "retouch_cover": null,
            "source": "default",
            "static_cover_image_path": "",
            "tracks": tracks,
            "update_time": 0,
            "version": 360000
        })
    }

    /// 构造 draft_meta_info.json 元数据结构
    pub fn build_draft_meta_info(draft_id: &str, draft_name: &str, duration_us: u64) -> Value {
        let now = Utc::now().timestamp_micros() as u64;
        json!({
            "draft_fold_path": "",
            "draft_id": draft_id,
            "draft_is_ai_shorts": false,
            "draft_is_invisible": false,
            "draft_json_file": "",
            "draft_name": draft_name,
            "draft_new_version": "",
            "draft_root_path": "",
            "draft_timeline_materials_size": 0,
            "draft_type": "",
            "tm_draft_cloud_completed": "",
            "tm_draft_cloud_modified": 0,
            "tm_draft_create": now,
            "tm_draft_modified": now,
            "tm_draft_removed": 0,
            "tm_duration": duration_us
        })
    }

    /// 导出草稿文件夹至任意指定路径
    pub fn export_to_folder<P: AsRef<Path>>(
        segments: &[Segment],
        video_path: Option<&Path>,
        target_dir: P,
        draft_name: &str,
        bilingual: bool,
    ) -> Result<PathBuf> {
        let target_dir = target_dir.as_ref();
        fs::create_dir_all(target_dir).with_context(|| "创建目标草稿文件夹失败")?;

        let total_dur = segments.last().map(|s| s.end).unwrap_or(0.0);
        let duration_us = (total_dur * 1_000_000.0) as u64;
        let draft_id = Uuid::new_v4().to_string().to_uppercase();

        let content_json = Self::build_draft_content(segments, video_path, total_dur, bilingual);
        let meta_json = Self::build_draft_meta_info(&draft_id, draft_name, duration_us);

        let content_path = target_dir.join("draft_content.json");
        let meta_path = target_dir.join("draft_meta_info.json");

        let mut content_file = File::create(&content_path)?;
        content_file.write_all(serde_json::to_string_pretty(&content_json)?.as_bytes())?;

        let mut meta_file = File::create(&meta_path)?;
        meta_file.write_all(serde_json::to_string_pretty(&meta_json)?.as_bytes())?;

        Ok(target_dir.to_path_buf())
    }

    /// 一键直接注入本机剪映草稿库（自动更新 root_meta_info.json，开箱即剪）
    pub fn inject_to_local_jianying(
        segments: &[Segment],
        video_path: Option<&Path>,
        base_name: &str,
        bilingual: bool,
    ) -> Result<PathBuf> {
        let draft_root = Self::detect_local_draft_root()
            .context("未在系统中探测到剪映专业版（JianyingPro）草稿安装目录，请先安装或启动一次剪映")?;

        let now_fmt = chrono::Local::now().format("%m月%d日_%H%M%S").to_string();
        let project_name = format!("{}_{}", base_name, now_fmt);
        let project_dir = draft_root.join(&project_name);

        fs::create_dir_all(&project_dir)?;

        let total_dur = segments.last().map(|s| s.end).unwrap_or(0.0);
        let duration_us = (total_dur * 1_000_000.0) as u64;
        let draft_id = Uuid::new_v4().to_string().to_uppercase();
        let now_us = Utc::now().timestamp_micros() as u64;

        let content_json = Self::build_draft_content(segments, video_path, total_dur, bilingual);
        let mut meta_json = Self::build_draft_meta_info(&draft_id, &project_name, duration_us);

        let norm_project_dir = project_dir.to_string_lossy().replace('\\', "/");
        let norm_draft_root = draft_root.to_string_lossy().replace('\\', "/");
        let norm_content_file = format!("{}/draft_content.json", norm_project_dir);

        meta_json["draft_fold_path"] = json!(norm_project_dir);
        meta_json["draft_json_file"] = json!(norm_content_file);
        meta_json["draft_root_path"] = json!(norm_draft_root);

        let content_path = project_dir.join("draft_content.json");
        let meta_path = project_dir.join("draft_meta_info.json");

        let mut content_file = File::create(&content_path)?;
        content_file.write_all(serde_json::to_string_pretty(&content_json)?.as_bytes())?;

        let mut meta_file = File::create(&meta_path)?;
        meta_file.write_all(serde_json::to_string_pretty(&meta_json)?.as_bytes())?;

        // 注册到剪映的 root_meta_info.json 中
        if let Some(root_meta_file) = Self::get_root_meta_path() {
            if let Ok(content) = fs::read_to_string(&root_meta_file) {
                if let Ok(mut root_val) = serde_json::from_str::<Value>(&content) {
                    let new_store_entry = json!({
                        "draft_cloud_last_action_download": false,
                        "draft_cloud_purchase_info": "",
                        "draft_cloud_template_id": "",
                        "draft_cloud_tutorial_info": "",
                        "draft_cloud_videocut_purchase_info": "",
                        "draft_cover": "",
                        "draft_fold_path": norm_project_dir,
                        "draft_id": draft_id,
                        "draft_is_ai_shorts": false,
                        "draft_is_invisible": false,
                        "draft_json_file": norm_content_file,
                        "draft_name": project_name,
                        "draft_new_version": "",
                        "draft_root_path": norm_draft_root,
                        "draft_timeline_materials_size": 0,
                        "draft_type": "",
                        "tm_draft_cloud_completed": "",
                        "tm_draft_cloud_modified": 0,
                        "tm_draft_create": now_us,
                        "tm_draft_modified": now_us,
                        "tm_draft_removed": 0,
                        "tm_duration": duration_us
                    });

                    if let Some(store) = root_val.get_mut("all_draft_store").and_then(|s| s.as_array_mut()) {
                        store.insert(0, new_store_entry);
                    }
                    if let Some(ids) = root_val.get_mut("draft_ids").and_then(|i| i.as_u64()) {
                        root_val["draft_ids"] = json!(ids + 1);
                    }

                    let _ = fs::write(&root_meta_file, serde_json::to_string_pretty(&root_val)?);
                }
            }
        }

        Ok(project_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jianying_draft_generation() {
        let segs = vec![
            Segment::new(1, 0.0, 2.5, "测试第一句字幕"),
            Segment::new(2, 2.6, 5.0, "测试第二句字幕"),
        ];

        let temp_dir = std::env::temp_dir().join("test_voice2word_draft");
        let result = JianYingExporter::export_to_folder(&segs, None, &temp_dir, "单元测试草稿", false);
        assert!(result.is_ok());

        let content_file = temp_dir.join("draft_content.json");
        let meta_file = temp_dir.join("draft_meta_info.json");
        assert!(content_file.exists());
        assert!(meta_file.exists());

        let content_str = fs::read_to_string(&content_file).unwrap();
        let val: Value = serde_json::from_str(&content_str).unwrap();
        assert_eq!(val["materials"]["texts"].as_array().unwrap().len(), 2);
        assert_eq!(val["tracks"].as_array().unwrap().len(), 1);

        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn content_xml_escapes_special_chars() {
        let segs = vec![Segment::new(1, 0.0, 1.0, r#"A&B<C>"引号'"#)];
        let val = JianYingExporter::build_draft_content(&segs, None, 1.0, false);
        let content = val["materials"]["texts"][0]["content"].as_str().unwrap();
        assert!(content.contains("A&amp;B&lt;C&gt;&quot;引号&apos;"), "未正确转义: {content}");
        assert!(!content.contains("A&B"), "原始特殊字符泄漏进了内嵌 XML");
    }


    /// 双语导出：译文必须出现在 draft_content.json 的 content 字段里
    /// （回归「剪映草稿丢译文」）。
    #[test]
    fn jianying_includes_translation_when_bilingual() {
        let mut seg = Segment::new(1, 0.0, 1.0, "你好世界");
        seg.translation = Some("Hello world".to_string());
        seg.translation_lang = Some("English".to_string());
        let segs = vec![seg];

        let val = JianYingExporter::build_draft_content(&segs, None, 1.0, true);
        let content = val["materials"]["texts"][0]["content"].as_str().unwrap();
        assert!(content.contains("你好世界"), "缺原文: {content}");
        assert!(content.contains("Hello world"), "双语草稿丢了译文: {content}");

        let raw = JianYingExporter::build_draft_content(&segs, None, 1.0, false);
        let content_raw = raw["materials"]["texts"][0]["content"].as_str().unwrap();
        assert!(!content_raw.contains("Hello world"), "关闭双语时不应出现译文");
    }
}

