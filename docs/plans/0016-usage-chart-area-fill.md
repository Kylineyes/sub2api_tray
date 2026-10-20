# 0016 修复用量曲线面积填充

| 字段 | 内容 |
| --- | --- |
| 编号 | 0016 |
| 分支 | fix/usage-chart-area-fill |
| 状态 | 已完成 |
| 提出日期 | 2026-10-20 |

## 一、背景

用量曲线使用 `egui::Shape::convex_polygon` 填充曲线与基线之间的面积。
折线起伏时该多边形不是凸多边形，egui 的凸多边形三角化会产生跨线三角面，
导致填充穿过折线、出现大片错误阴影。

## 二、目标

曲线面积严格限制在折线与基线之间，任何起伏都正确填充。

## 三、范围

包含：

* 用逐段三角 Mesh 替代 convex_polygon；
* 补充面积网格单测；
* 实机检查 24 小时 / 7 天 / 30 天曲线。

不包含：

* 不改变曲线数据或指标算法；
* 不引入图表库。

## 四、完成点

* [x] 面积填充改为 Mesh
* [x] 空数据/单点安全
* [x] 单元测试与实机验证
* [x] 文档登记

## 五、验收标准

1. 展开用量区 -> 面积只填充在折线下方。
2. 曲线上下起伏时 -> 不出现跨折线的大三角形或异常重叠。
3. 切换 24 小时 / 7 天 / 30 天 -> 填充均正常。
4. `cargo test` -> 网格顶点/三角形数量符合预期。

## 六、测试方式

* cargo fmt --check
* cargo clippy --all-targets -- -D warnings
* cargo test --locked
* cargo build --release --locked
* Windows 实机截图检查

## 七、风险与回滚

| 风险 | 影响 | 应对 |
| --- | --- | --- |
| 三角形绕序导致不显示 | 面积缺失 | 实机截图确认；Mesh 颜色为双面渲染 |
| 相邻三角形接缝 | 出现细线 | 同一 Mesh 内共享顶点，不重复叠加 |

回滚方式：git revert 合并提交。

## 八、涉及文件

* src/app.rs
* docs/plans/0016-usage-chart-area-fill.md

## 九、结果

* 完成日期：2026-10-20
* 实际验证记录：
  * `cargo fmt --check` 通过；
  * `cargo clippy --all-targets -- -D warnings` 通过；
  * `cargo test --locked`：105 passed / 6 ignored / 0 failed；
  * `cargo build --release --locked` 通过；
  * Windows 实机放大用量区确认：多个起伏区间均只填充在折线与基线之间，无跨线三角面。
* 与计划的偏差：无。
* 遗留问题 / 后续计划编号：无。
