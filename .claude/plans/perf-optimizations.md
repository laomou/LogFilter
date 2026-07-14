# 性能优化实施计划：level_color 预解析 + Picker 选项缓存

## 1. `level_color` — 预解析颜色为数组

**现状** (`app.rs:1068-1076`)：
```rust
fn level_color(lv: LevelMask, cfg: &Config) -> Color32 {
    let s = if lv.contains(LevelMask::F) { &cfg.colors.level_f } ...;
    parse_color(s)  // 每个单元格都做字符串解析
}
```
50 可见行 × 50fps = 2500 次/秒 `parse_color` 调用。

**修法**：
- `App` 新增 `cached_level_colors: [Color32; 6]`
- `App::new` 中从 config 预解析 6 个颜色
- `refresh_highlight_caches` 中检测 level colors 配置是否变化（rare），变化时更新缓存
- `level_color` 改为查表：`cached_level_colors[level_index(lv)]`

---

## 2. Picker 面板 — 避免每帧重建选项列表

**现状** (`app.rs:851-890`)：
```rust
// 每帧都 clone 所有 key + 排序
let mut v: Vec<(String, usize)> = m.tag_counts.iter()
    .map(|(k, &v)| (k.clone(), v)).collect();
v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
```

**修法**：
- `App` 新增 3 个字段：
  - `cached_picker_col: Option<PickerCol>` — 上次缓存的列类型
  - `cached_picker_options: Vec<(String, usize)>` — 缓存的选项列表
  - `cached_picker_entries_len: usize` — 缓存时的 entries 数量（作为简易版本号）
- `render_picker` 中：若 `picker.col` 与缓存列类型相同且 `model.entries.len()` 与缓存版本号一致 → 复用；否则重建并更新缓存
- `clear()` 和 `open_file()` 会清空 Model（entries.len() = 0），版本号自动失效

---

## 涉及文件
- 仅 `src/app.rs`，两个独立改动无冲突

## 影响范围
- `level_color` 改为非泛型查表，不影响调用方
- `render_picker` 选项构建改为惰性一次计算，UI 行为不变
