# 交互优化实施计划：键盘导航 + Goto + 窗口持久化

## 改动范围

仅修改 `src/app.rs`，涉及以下 4 项：

---

## 1. 上下键逐行导航 (ArrowUp/ArrowDown)

**知识点**：`handle_shortcuts` 已处理 `PageUp`/`PageDown`，只需在 `Key::PageDown` 处理之后增加 `ArrowUp`/`ArrowDown` 两项。

**代码变更**：
- `handle_shortcuts` 中增加：
  ```rust
  // ArrowUp/ArrowDown: move selection by 1 row
  if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
      self.move_selected_row(-1);
      return;
  }
  if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
      self.move_selected_row(1);
      return;
  }
  ```
- 新增 `fn move_selected_row(&mut self, delta: isize)` — 移动单行选中并更新 `selection_anchor`

---

## 2. Shift+上下键扩展选择范围

**知识点**：鼠标 Shift+click 已有范围选择逻辑（`app.rs:1863-1867`），需要：
- 新增 `selection_anchor: Option<usize>` 字段到 `App`
- 箭头键处理中检测 Shift：Shift+Arrow 调用 `extend_selection`，否则调用 `move_selected_row`
- 鼠标点击时同步更新 anchor（普通点击重置 anchor，Shift+click 扩展）

**代码变更**：
- `App` 结构体新增：`selection_anchor: Option<usize>`
- `App::new` 初始化：`selection_anchor: None`
- `handle_shortcuts` 中 ArrowUp/ArrowDown 改为：
  ```rust
  let shift = ctx.input(|i| i.modifiers.shift);
  if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
      if shift { self.extend_selection(-1); } else { self.move_selected_row(-1); }
      return;
  }
  ```
- 新增 `fn extend_selection(&mut self, delta: isize)` — 以 anchor 为基准扩展选择范围
- `ui_table` 的点击处理中（`app.rs:1868-1870`），普通点击时追加 `self.selection_anchor = Some(r);`

---

## 3. Goto 行号改为 Enter 确认跳转

**知识点**：当前 `goto_resp.changed()` 时立即跳转（`app.rs:1415-1418`），用户输入过程中会连续跳转。改为在 TextEdit 失去焦点且按下 Enter 时跳转，避免误跳。

**代码变更**：
- `app.rs:1415-1418` 改为：
  ```rust
  if goto_resp.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
      if let Ok(n) = self.ui.goto_line.trim().parse::<usize>() {
          if n > 0 { goto_target = Some(n - 1); }
      }
  }
  ```

---

## 4. 窗口大小持久化

**知识点**：`Config` 已有 `window.width/height` 字段，`write_back` 只保存了过滤/编码，未保存窗口尺寸。`on_exit` 中无法访问 egui context，需在 `ui()` 中捕获窗口尺寸，`on_exit` 时写入。

**代码变更**：
- `App` 结构体新增：`last_window_size: Option<egui::Vec2>`
- `App::new` 初始化：`last_window_size: None`
- `ui()` 函数开头（`handle_shortcuts` 之后）捕获：`self.last_window_size = ctx.input(|i| i.viewport().inner_rect).map(|r| r.size());`
- `UiState::write_back` 增加：
  ```rust
  // 窗口尺寸由 App 层写入（UiState 不持有尺寸）
  ```
  — 不对，`write_back` 在 `UiState` 上，窗口尺寸在 `App` 上。需要改为在 `on_exit` 中直接写入：
  ```rust
  fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
      self.ui.write_back(&mut self.cfg);
      if let Some(size) = self.last_window_size {
          self.cfg.window.width = size.x;
          self.cfg.window.height = size.y;
      }
      let _ = config::save(&self.cfg);
  }
  ```