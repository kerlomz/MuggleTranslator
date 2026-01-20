# MuggleTranslator 翻译重构计划（工业级架构，DOCX 100% 还原优先）

> 目标：只做 **中文↔英文**（zh→en / en→zh）双向翻译，保证 DOCX 格式回填“原汁原味”，并把“拼接感”降到可接受的最低。

---

## 0. 现状基线（已具备的“格式拆解三件套”）

当前仓库已经具备稳定的 **纯规则 DOCX 拆解/还原** 能力（无 LLM 参与解析），并通过 `test.docx` 的严格验证：

- `*.text.json`：包含
  - `paragraphs[]`：对齐 python-docx 的段落语义抽取（用于翻译输入/覆盖率对齐）
  - `slot_texts[]`：全量文本槽位（用于合并回填）
- `*.offsets.json`：**仅槽位定位**（part + event_index + kind + attr），不含真实文本
- `*.mask.json` + `*.mask.blobs.bin`：DOCX zip 的“骨架 + 字节仓库”
  - mask 内 XML 的 Text/CData 与 `w:lvlText@w:val` 已替换为占位符
  - 实际字节放入 `mask.blobs.bin`，mask.json 仅存引用（offset/len/sha256），更可读更小
- `*.structure.json`：树状结构（heading/list-aware），便于理解原文层级与上下文

**关键结论**：翻译阶段只需要“改动 `text.json.slot_texts` 中可翻译槽位”，然后用 `mask+offsets+text` 合并，即可得到格式稳定的 `*.docx`。

---

## 1. 总体目标与不可妥协约束

### 1.1 目标

- **zh→en / en→zh**：仅支持两种方向，围绕这两种方向把提示词、质量检查、标点/数字规则做深优化。
- **整文档覆盖**：任何段落不允许“跳过而不知情”；翻译覆盖率与缺失必须可审计。
- **格式 100% 还原优先**：所有不属于“真实可见文本”的结构、标签、资源字节不变。
- **低拼接感**：通过上下文、术语一致性、后处理与修补循环降低“段落间断裂感”。

### 1.2 不可妥协约束（工程约束）

- DOCX 结构解析/提取/还原：**必须纯规则**（禁止用大模型解析结构）。
- 任何“回填到 DOCX”的行为：必须通过可验证的规则与映射字典实现，不允许“猜测式回写”。
- 单文件规模：保持模块拆分，避免超大文件（目标：每个文件 < 1000 行）。

---

## 2. 重构后的工业级架构（模块化、可拔插、可扩展）

### 2.1 分层设计

1) **Docx Layer（格式层）**  
负责：拆解/还原/定位/验证，输出与翻译无关的稳定工件。

2) **Text Layer（语义层）**  
负责：把 `slot_texts` 与 `paragraphs`/`structure` 组合成“可翻译单元（TU）”，并生成严格可回填的映射。

3) **Model Layer（模型层）**  
负责：统一封装 llama.cpp 调用（本地/服务端），并定义可切换的“翻译模型/重写模型/裁判模型/Embedding”接口。

4) **Prompt Layer（提示词层）**  
负责：提示词从文件加载、可版本化、可生成默认配置；分阶段模板（翻译/修补/裁判/总结等）。

5) **Pipeline Layer（流水线层）**  
负责：翻译 A、翻译 B、裁判融合、全局拼接感检测、问题段落修补、落盘与恢复。

6) **IO Surface（对外接口）**  
负责：DLL（核心）+ EXE 外壳（拖拽 docx），以及 CLI（开发/验证/批处理）。

---

## 3. 工件（Artifacts）与 JSON 设计（可读、可审计、可恢复）

> 核心策略：**翻译只改动可翻译槽位**，其余结构/字节完全不动。

### 3.1 格式层工件（已存在，继续保留）

- `doc.mask.json`（版本 2）  
  - `entries[]`：zip 条目元信息 + `External(offset,length,sha256)` 引用
  - `blobs_file`：指向 `doc.mask.blobs.bin`
- `doc.mask.blobs.bin`  
  - 顺序写入每个 zip entry 的字节（XML 为占位符版）
- `doc.offsets.json`（版本 1）  
  - `slots[]`：`{id, part_name, kind, event_index, attr_name}`
- `doc.text.json`（版本 3）  
  - `slot_texts[]`：所有槽位原文（合并回填使用）
  - `paragraphs[]`：对齐 python-docx 的段落抽取（翻译输入与覆盖率核对）
- `doc.structure.json`（版本 1）  
  - `root` 树：`heading/list/list_item/paragraph`，用于上下文组织

### 3.2 新增：定位映射工件（翻译落地所必需）

为实现“段落级翻译 → 精确回填到槽位”，需要新增一个 **mapping 工件**（建议文件名）：

- `doc.map.json`（新）  
  - `para_to_slot_ids[]`：每个 `para_id` 对应一组“可翻译槽位 id 列表”（按原文顺序）
  - `slot_flags[]`：每个 slot 的分类/标记（可翻译/不可翻译/纯空白/纯标点/数字域/目录点线等）
  - `para_path[]`：该段落在 `structure` 中的路径信息（heading 路径、list 层级）

> 这个文件把“翻译输入（段落/片段）”与“回填目标（slot id）”强绑定，是后续所有工程可靠性的基础。

### 3.3 新增：翻译记忆与可恢复工件（建议）

- `doc.memory.json`（新）  
每段一个记录，建议字段：
  - `para_id`, `scope_key`, `direction`
  - `source_text`
  - `context`（heading path、邻接段、术语表摘要）
  - `glossary_hits`（术语命中、强制翻译/不翻译策略）
  - `translation_a`, `translation_b`, `final`
  - `quality_flags`（占位符/数字/标点/长度异常/疑似拼接感）
  - `repair_notes`（修补要求与历史）

> 目的：断点续跑、可审计、可对比模型输出差异、便于回归测试。

---

## 4. 模型层统一封装（可切换、可扩展、可对齐 LMStudio 性能）

### 4.1 模型角色（固定角色 + 可选扩展）

**主翻译模型（Translation-A）**
- `translategemma-4b-it.i1-Q5_K_S.gguf`（默认，快）
- `translategemma-12b-it.i1-Q6_K.gguf`（更强，更稳）

**备选翻译模型（Translation-B）**
- `HY-MT1.5-1.8B-Q8_0.gguf`（高效翻译专用；当前“只有引擎实现”，按计划补齐调用与集成）

**裁判/融合/润色模型（Decision/Judge/Polish，可选）**
- `gemma-3-4b-it.Q6_K.gguf`：对比 A/B 输出 + 上下文，输出最终融合版本；也可用于“拼接感诊断”与“修补要求生成”
- `gemma-3-1b-it.Q6_K.gguf`：低成本辅助（例如语言检测、轻量一致性检查、快速术语候选）

**Embedding（可选）**
- `embeddinggemma-300m-qat-Q4_0.gguf`：术语库/相似段召回、上下文检索、跨段一致性辅助

### 4.2 统一接口（建议）

- `ModelClient`（基础）：`generate(prompt, params) -> completion`
- `ChatClient`（可选）：面向 instruct 模型的 role 消息
- `EmbedClient`：`embed(texts) -> vectors`

支持后端：
- `llama.cpp local`（llama-cpp-rs）
- `llama.cpp server`（HTTP，便于持久化加载、吞吐更高）
- `LMStudio OpenAI-compatible`（若需要复用其成熟的运行参数与缓存策略）

### 4.3 性能参数对齐（向 LMStudio 看齐）

模型配置需要在 `toml` 可配置并可导出默认值（典型参数）：
- `n_ctx`（翻译/裁判分别配置）
- `n_gpu_layers`（尽量全 offload）
- `n_threads`
- `n_batch` / `n_ubatch`
- `flash_attn` / `kv_cache_on_gpu`（如后端支持）
- `mmap` / `mlock` / `keep_in_memory`

并提供 `perf profile`：同一段落集的 tokens/s、首 token 延迟、吞吐稳定性。

---

## 5. 提示词体系（文件化、可版本化、最小约束）

### 5.1 提示词来源与管理

- 所有提示词模板必须从文件读取（例如 `prompts/*.md` 或 `prompts/*.txt`）。
- CLI 支持 `--init-config` 生成默认 prompts，方便迭代与版本对比。
- 每个阶段一个模板文件：翻译、裁判融合、修补、拼接感诊断、术语提取等。

### 5.2 翻译提示词原则（避免过度约束）

- 只约束必要格式：**不得输出解释/前后缀**；若使用占位符/标记，要求“原样保留、不增不减、不改顺序”。
- 只做 zh↔en：提示词可更短、更稳定（减少多语言分支）。

---

## 6. 翻译单元（TU）与回填策略（核心工程点，提供可选方案）

### 6.1 TU 定义（建议）

TU = “段落级语义单元”，包含：
- `para_id` + `scope_key`（定位）
- `direction`（zh→en / en→zh）
- `source_text`（来自 `paragraphs[].text`）
- `slot_ids[]`（来自 `doc.map.json`）
- `context`（来自 `structure` + 邻接段 + 术语召回）

### 6.2 回填策略（提供 3 种可选路径）

#### 方案 A：Slot Sentinel 翻译（强格式稳定，推荐默认）

- 把一个段落拆成 slot 片段序列（只含可翻译 slot），用轻量标记包裹：  
  `<<S0001>>片段1<<S0002>>片段2...`
- 翻译模型要求：  
  - 只输出翻译结果  
  - 所有 `<<Sxxxx>>` 原样保留、顺序不变
- 解析输出：按标记切回每个 slot 的译文，写入 `slot_texts[slot_id-1]`

优势：回填稳定、无需“对齐算法”；格式几乎不受影响。  
代价：模型可能在“分片”处产生轻微断裂，需要上下文 + 后修补。

#### 方案 B：段落整体翻译 + 对齐分配（质量潜力更高，工程复杂）

- 让模型输出整段译文
- 使用规则/统计对齐算法（非 LLM）把译文分配回 slot（按原 slot 字符数、标点、空格位置等）

优势：译文更连贯；无需标记。  
代价：对齐算法难、失败概率高；需要复杂 fallback。

#### 方案 C：混合策略（工程稳 + 质量上限）

- 默认使用 A 保证格式稳定
- 当段落 slot 数量很少/样式简单时，尝试 B（更自然）
- B 失败自动回退 A；A 失败触发“修补 loop”

---

## 7. 多阶段质量策略（降低拼接感的工程化路径）

> 目标：把“低拼接感”变成可工程化的闭环，而不是靠一次翻译赌运气。

### 7.1 版本 A / 版本 B

- A：主翻译（translategemma 4b/12b 二选一）
- B：备选翻译（HY-MT 1.8B Q8_0）

### 7.2 裁判融合（可选，但建议保留接口）

- 输入：原文 + A + B + 上下文摘要/术语约束
- 输出：只输出最终译文（不输出解释/JSON）
- 角色：`gemma-3-4b-it.Q6_K.gguf`（当前已知较适配）

### 7.3 全局拼接感诊断 + 局部修补

1) 把整文档（或大窗口分块）喂给 gemma-3-4b：输出“段落 id + 问题描述 + 修补要求”  
2) 对问题段落调用 `translategemma-12b` 进行“上下文重译/润色”，回填覆盖  
3) 再跑一次诊断，直到问题清零或达到可接受阈值

---

## 8. 术语与一致性（可选增强项，优先保证稳定与可控）

### 8.1 术语抽取（规则优先 + 模型辅助可选）

- 规则：大写缩写、法条引用、定义句式（“means/shall mean”）、专有名词等
- 模型（可选）：gemma-3-4b 提取候选术语对（仅做建议，不参与结构解析）

### 8.2 Embedding 检索（embeddinggemma-300m，可选）

- 建立段落向量索引（文档内）
- 每段翻译时召回相似段落的译法，做一致性提示（尤其法务文本）

---

## 9. 工程化验证（必须内建到流水线）

### 9.1 格式与回填验证

每次翻译输出 docx 必须自动跑：
- `verify_docx_roundtrip`（结构/字节级校验）
- python-docx 对比（段落/表格/页眉页脚抽取对齐）

### 9.2 翻译覆盖率与缺失检测

必须输出覆盖率统计：
- `paragraphs_total` / `paragraphs_translated`
- 空译文/重复译文/明显异常长度
- 标记/占位符丢失（方案 A 的强一致性检查）

### 9.3 质量规则（规则优先）

- 数字/金额/日期保真
- 引号/括号/标点配对
- 超短/超长异常
- 大段落被截断（max tokens/stop 序列误触发）

---

## 10. 对外交付形态（DLL + EXE 外壳 + CLI）

### 10.1 DLL（核心翻译库）

提供稳定 C ABI（示例）：
- `mt_init(config_path)`
- `mt_translate_docx(input_path, output_path, direction, options_json)`
- `mt_last_error()`

### 10.2 EXE（拖拽外壳）

只做：
- 拖拽 docx -> 调 DLL -> 输出 docx
- 显示进度与错误

### 10.3 CLI（开发/回归/批处理）

必须具备：
- `--verify-extract-merge-json`（格式层回归）
- `--extract-structure-json`
- `translate --model ... --direction ... --workdir ...`（可断点续跑）

---

## 11. 观测性（trace/日志/可复现）

- 统一 `_trace/<job_id>/`：
  - `inputs/`：提示词、片段、上下文
  - `outputs/`：模型输出原文
  - `memory.json`：段落级状态机
  - `report.md`：覆盖率与异常汇总
- 日志必须区分：解析、TU 构建、模型调用、回填、验证。

---

## 12. 实施优先级（不改需求，仅按依赖关系排序）

1) **TU 映射 doc.map.json**：打通“段落 ↔ slot_ids”与“可翻译槽位筛选”
2) **模型统一封装 + 配置化 prompts**：确保可快速切换 translategemma-4b/12b 与 HY-MT
3) **方案 A（Slot Sentinel）翻译落地**：先保证“全段落可翻译 + 可回填 + 全文覆盖”
4) **版本 B + 裁判融合接口**：把 A/B 与 gemma-3-4b 融合变成可插拔模块
5) **拼接感诊断与修补闭环**：让“流畅”变成可工程化重复的迭代流程
6) **Embedding/术语一致性增强**：在稳定后再加（避免前期引入不确定性）

---

## 13. 给你们的选择点（可配置开关）

- 主翻译：`translategemma-4b`（快） vs `translategemma-12b`（稳）
- 备翻译：启用/禁用 HY-MT（在调用补齐后）
- 回填策略：A（默认）/ B / C
- 裁判融合：启用/禁用 gemma-3-4b
- 拼接感修补：启用/禁用；阈值与迭代次数可配
- Embedding 检索：启用/禁用

---

## 14. 代码结构重构落地（明确“抛弃旧翻译逻辑”的拆分方案）

> 目标：把“翻译流水线”与“DOCX 格式层”彻底解耦；模型调用、提示词、质量规则、落盘恢复全部模块化。

建议把当前翻译相关的旧逻辑（现有 `src/pipeline/*`、旧的 agent/拼接逻辑等）按角色拆解/替换为以下模块（名称仅为建议，按现有仓库风格落地即可）：

- `src/docx/*`：保留（格式层已稳定）
  - `decompose.rs / pure_text.rs / structure.rs / package.rs / xml.rs`
- `src/translate/*`：新（翻译流水线主入口）
  - `job.rs`：Job/Workdir/断点续跑
  - `tu.rs`：TU 数据结构 + 构建
  - `map.rs`：`doc.map.json` 生成（段落↔slot 映射 + slot flags）
  - `apply.rs`：把 TU 结果写回 `text.json.slot_texts`
  - `pipeline.rs`：A/B/裁判/修补/全局诊断调度（只做 orchestration，不做细节）
- `src/models/*`：新（统一模型封装）
  - `client.rs`：`ModelClient/EmbedClient` trait
  - `llamacpp_local.rs`：本地推理
  - `llamacpp_server.rs`：HTTP 推理（推荐用于持久加载与性能）
  - `openai_compat.rs`：LMStudio/OpenAI 兼容（可选）
- `src/prompts/*`：新（提示词加载与渲染）
  - `loader.rs`：从文件加载模板、变量替换
  - `registry.rs`：按阶段管理模板（translate/judge/repair…）
- `src/quality/*`：新（规则化质量检查）
  - `checks.rs`：占位符/数字/标点/截断/空译文/长度异常
  - `stitch.rs`：拼接感规则诊断（非 LLM）
- `src/ffi.rs`：保留并精简（DLL 对外接口只调用 `translate::job`）
- `src/main.rs`：CLI/拖拽壳（只做参数解析与调用库）

约束：每个文件明确职责，避免“一个文件写完所有事”；公共逻辑必须下沉到可复用模块。

---

## 15. 配置文件（TOML）草案（工业级可控参数面）

建议把“模型/提示词/策略/质量阈值”全部放到 `muggle-translator.toml`，并提供 CLI 生成默认值。

示例（草案）：

```toml
[job]
direction = "auto"                 # "zh2en" | "en2zh" | "auto"
content_scope = "python_docx_compatible"  # 见第 18 节
workdir = "_trace"
max_concurrency = 1

[strategy]
fill_mode = "sentinel_slots"       # "sentinel_slots" | "align" | "hybrid"
enable_translation_b = false
enable_judge = true
enable_global_stitch_check = true
max_repair_rounds = 2

[models.translation_a]
backend = "llamacpp_server"        # "llamacpp_local" | "llamacpp_server" | "openai_compat"
model = "translategemma-4b-it.i1-Q5_K_S.gguf"
ctx = 8192
gpu_layers = 999
threads = 12
batch = 512
temperature = 0.2
top_p = 0.9
repeat_penalty = 1.05

[models.translation_b]
backend = "llamacpp_server"
model = "HY-MT1.5-1.8B-Q8_0.gguf"
enabled = false

[models.judge]
backend = "llamacpp_server"
model = "gemma-3-4b-it.Q6_K.gguf"
enabled = true
ctx = 32768
temperature = 0.1

[models.embedding]
backend = "llamacpp_server"
model = "embeddinggemma-300m-qat-Q4_0.gguf"
enabled = false

[prompts]
translate_zh2en = "prompts/translate_zh2en.txt"
translate_en2zh = "prompts/translate_en2zh.txt"
judge_merge = "prompts/judge_merge.txt"
stitch_report = "prompts/stitch_report.txt"
repair = "prompts/repair.txt"

[quality]
min_output_chars = 2
max_length_ratio = 3.5
require_all_sentinels = true
```

---

## 16. 提示词模板清单（文件化，最小约束版本）

建议最少准备这些模板文件（均要求“只输出正文，不加解释”）：

- `prompts/translate_zh2en.txt`
- `prompts/translate_en2zh.txt`
- `prompts/judge_merge.txt`（输入：原文 + A + B + 关键上下文；输出：最终译文）
- `prompts/stitch_report.txt`（输入：整文档/大窗口；输出：段落 id + 问题描述 + 修补要求）
- `prompts/repair.txt`（输入：原文 + 上下文 + 问题要求；输出：修补后译文）

以及开发调试专用（不会进入生产默认流）：

- `prompts/dev_sentinel_stress.txt`（专测标记丢失/顺序变动）
- `prompts/dev_long_context.txt`（专测长窗口与段落划分）

---

## 17. `doc.map.json`（段落↔槽位）生成算法（纯规则）

`text.json.slot_texts` 是“全量文本槽位”，其中包含大量不可翻译内容（XML 缩进空白、无意义换行、资源/关系文件中的文本等）。因此必须新增一层纯规则映射：

1) 以 `offsets.json.slots` 的顺序为准，建立 `slot_id -> (part_name, event_index, kind, attr_name)`。
2) 对每个 XML part 重新解析成事件流，并维护 element stack（`w:p`、`w:r`、`w:t`、`w:hyperlink` 等）。
3) 当进入一个 `w:p` 时开始捕获，并按“可见文本语义”挑选槽位：
   - 默认对齐 python-docx 语义：只收集 `w:p` 直接子节点 `w:r/w:t` 与 `w:hyperlink/w:r/w:t` 中的 `w:t` 文本槽位。
   - 记录该段落命中的 `slot_ids[]`（顺序必须与可见文本串联顺序一致）。
4) 生成 `slot_flags[]`：
   - `translatable=true`：属于 `w:t`（可见正文）
   - `translatable=false`：XML 缩进空白、关系文件、Content_Types、以及不在可见路径中的文本等
5) 生成 `para_path[]`：从 `structure.json` 反查每个 `para_id` 的 heading/list 路径，作为上下文组织基础。

> 这一层会把“翻译的目标范围”明确落到 slot 粒度，避免误翻译 XML 空白或元数据导致不可预期问题。

---

## 18. 内容覆盖范围（Content Scope）作为开关交付

因为 DOCX 的“可见文本”不止存在于 body/table/header/footer 的直接段落，实际还可能存在于：
- 形状/文本框（`w:drawing/wps:txbx/w:txbxContent`）
- 批注（comments）
- 脚注/尾注（footnotes/endnotes）
- 文本替代（alt text）、标题、书签内容等

但这些内容有的属于“装饰/水印/页码域”，翻译会带来风险。因此建议把覆盖范围做成可选开关：

- `python_docx_compatible`（默认，最稳）：与当前 `paragraphs[]` 抽取一致
- `include_textboxes`：额外包含文本框内容（谨慎开启）
- `include_footnotes_endnotes`：补齐注释体系（法务文件常见）
- `include_comments`：是否翻译批注（通常可关闭）
- `full_visible_text`：尽可能覆盖所有可见文本（需要更严格 QA 与回归）

每种 scope 都必须有“覆盖率统计 + 可回归验证”，避免“看似翻完，实际漏翻”。

---

## 19. “低拼接感”细化为可执行工程策略

除了第 7 节的多阶段策略，本节补齐可落地的工程细节：

- 上下文包（Context Pack）：
  - heading 路径（从 `structure.json` 提取）
  - 邻接段（上一段/下一段，按 token 预算裁剪）
  - 术语约束（规则/embedding 召回）
  - 风格提示（法律文本/合约条款/标题/表格单元格）
- 拼接感规则检测（非 LLM）：
  - 连接词/指代词异常（e.g. “this”, “the foregoing” 指代缺失）
  - 段落开头重复/断裂
  - 同一术语多译（基于术语表/embedding）
- 修补指令生成（LLM 可选）：
  - `gemma-3-4b` 输出“问题段落 id + 修补要求”
  - `translategemma-12b` 按要求重译

---

## 20. 失败恢复与可重入（必须具备）

流水线必须做到“随时中断可恢复”，避免长文档翻译中途失败导致重跑：

- 每个 TU 的状态落盘：`pending -> translated_a -> translated_b -> judged -> applied -> verified`
- 任何一次模型输出都要保存原始输出文本（便于复现与修 prompt）
- 输出 docx 前后：都要记录 hash/统计，确保可对比

---

## 21. 完成标准（Definition of Done）

- **格式层**：`mask+offsets+text -> restored.docx` 可 100% 复原（已具备，需持续回归）
- **翻译覆盖**：在选定 `content_scope` 下，`paragraphs_total == paragraphs_translated`，且无 silent skip
- **回填正确**：占位符/标记不丢失；slot 写回不越界；merge 无 leftover placeholder
- **质量门槛**：输出无空译文/无截断/数字与关键符号保真；拼接感问题可通过修补闭环显著下降
- **性能可控**：能复现实测 LMStudio 的关键参数组合，吞吐不出现数量级差距
