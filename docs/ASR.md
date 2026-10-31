# 本地语音识别（ASR）可行性

> 状态：**调研完成，未实测**。目标板：进迭时空 K1（riscv64 / 8GB）。
> 结论先行：**能跑，走 CPU + RVV；不要指望 NPU。** 但性能必须实测，别按峰值估算。
> 相关：`VOICE.md`（语音消息的接入形态）、`WEIXIN.md`（微信自带转写已确认可用）。

## 1. 硬件事实

| 项 | 值 | 影响 |
|---|---|---|
| SoC | SpacemiT K1，8× X60 @ ~2GHz | 多核可并行，但 ASR 常受**内存带宽**限制 |
| 向量 | **RVV 1.0**（256-bit） | 是唯一可用的加速路径 |
| "2 TOPS" | **RVV 的 INT8 峰值**，不是独立 NPU | 峰值≠实测；ASR 到不了这个数 |
| 内存 | 8GB（服务本身 `MemoryMax=1G`） | ASR 应独立成 unit，别挤进主服务的 cgroup |

**没有可用的 NPU 路径**：sherpa-onnx 支持的 NPU 只有 Rockchip RKNN / Qualcomm QNN / Ascend /
Axera / Intel OpenVINO，**没有 SpacemiT**。所以 `-march=rv64gcv` 的 CPU 构建就是唯一正解。

## 2. 生态现状（已核实）

| 事实 | 证据 |
|---|---|
| sherpa-onnx 有 **SpacemiT 专用构建** | release 资产 `sherpa-onnx-v2.0.1-linux-riscv64-spacemit-shared.tar.bz2`（21MB） |
| 编译选项对准 K1 | `toolchains/riscv64-linux-gnu-spacemit.toolchain.cmake`：`-march=rv64gcv_zfh_zvfh_zba_zicbop_zihintpause -mabi=lp64d -ftree-vectorize` |
| 用 SpacemiT 优化的 onnxruntime | 仓库有 `cmake/onnxruntime-linux-riscv64-spacemit.cmake` |
| whisper.cpp 也能编 | ggml 有 `src/ggml-cpu/arch/riscv/{cpu-feats,quants,repack}` |

## 3. 模型选型

| 模型 | 中文 | 速度 | 备注 |
|---|---|---|---|
| **SenseVoice**（推荐） | 好 | **最快**（非自回归，一次前向） | 中英日韩粤；sherpa-onnx 原生支持 |
| Paraformer-zh | 最好 | 中 | FunASR 系；中文最准，但慢一档 |
| whisper tiny/base | 一般 | 慢 | 自回归解码，CPU 上 RTF 容易 >1；中文断句差 |

短语音（3–30 秒）场景优先 SenseVoice int8：
`sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09.tar.bz2`（在 k2-fsa/sherpa-onnx 的
`asr-models` release 里）。

## 4. 实测步骤（在板子上跑，约 30 分钟）

```bash
# 1) 二进制（选 spacemit 构建）
wget https://github.com/k2-fsa/sherpa-onnx/releases/download/v2.0.1/sherpa-onnx-v2.0.1-linux-riscv64-spacemit-shared.tar.bz2
tar xjf sherpa-onnx-v2.0.1-linux-riscv64-spacemit-shared.tar.bz2

# 2) 模型
wget https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09.tar.bz2
tar xjf sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09.tar.bz2

# 3) 测试音频
#    优先用模型包里自带的 test_wavs/zh.wav（已经是 16k 单声道，不需要任何转换工具）
#    想用自己的录音再转（需要 ffmpeg）：
#      ffmpeg -i test.m4a -ar 16000 -ac 1 -c:a pcm_s16le test10s.wav
ls sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09/test_wavs/   # 一般自带 zh.wav / en.wav

# 4) 计时：**用 bash 内建 time**（不需要装 time 包）
BIN=./sherpa-onnx-v2.0.1-linux-riscv64-spacemit-shared/bin/sherpa-onnx-offline
MDL=sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09
WAV=$MDL/test_wavs/zh.wav
DUR=$(python3 -c "import wave,sys; w=wave.open(sys.argv[1]); print(w.getnframes()/w.getframerate())" "$WAV")

for t in 4 8; do
  TIMEFORMAT="threads=$t  %3R s"      # %3R = 墙钟秒数，3 位小数
  time "$BIN" \
    --sense-voice-model=$MDL/model.int8.onnx \
    --tokens=$MDL/tokens.txt \
    --sense-voice-use-itn=1 \
    --num-threads=$t \
    "$WAV"
done
```

> `TIMEFORMAT` + `time` 是 **bash 内建**，不依赖 `/usr/bin/time`（那是 `time` 包）。
> 如果连 `python3` 都想省，用 bash 5 的 `$EPOCHREALTIME` 也能自己算：
> `t0=${EPOCHREALTIME/./}; ...; t1=${EPOCHREALTIME/./}; echo $(( (t1-t0)/1000 ))ms`
>
> 如果模型包里**没有** `test_wavs/`（版本差异），就用 `ffmpeg -ar 16000 -ac 1` 转一段自己的录音；
> `ffmpeg` 也装不上时，用手机录一段 16k wav，或让另一台机器转好再拷过来。

**实测结果（2026-10-31，K1 / 8GB / SenseVoice int8 / 5.592 s 音频 / `--sense-voice-language=zh`）**：

| 线程 | 解码 | RTF | 相对 1 线程 | 含加载总墙钟 |
|---:|---:|---:|---:|---:|
| 1 | 4.397 s | 0.786 | 1.00× | 21.1 s |
| 2 | 3.063 s | 0.548 | 1.44× | 20.0 s |
| **4** | **2.431 s** | **0.435** | **1.81×** | 19.3 s |
| 8 | 7.355 s | 1.315 | **0.60×（倒退）** | 24.3 s |

- **4 线程是拐点**：8 线程比 4 线程慢 3 倍，典型的多核争抢内存带宽。
- **模型加载 16.2 s**（每次冷启动），所以必须常驻。
- 常驻 RSS ≈ **292 MB**（`top` 实测），不需要动主服务的 `MemoryMax=1G`。

**RTF = 耗时 ÷ 音频时长**（10 秒音频跑 3 秒 → RTF 0.3）。要自动算：

```bash
TIMEFORMAT='%3R'
el=$( { time "$BIN" --sense-voice-model=$MDL/model.int8.onnx --tokens=$MDL/tokens.txt \
        --num-threads=4 "$WAV" >/dev/null; } 2>&1 )
python3 -c "print(f'RTF={$el/$DUR:.2f}  ({$el:.2f}s / {$DUR:.1f}s)')"
```

| RTF | 结论 |
|---|---|
| < 0.3 | 很好，实时对话无感 |
| 0.3–0.8 | 可用但能感知；只在"离线/隐私"是硬需求时才值得 |
| > 0.8 | 放弃本地，用云 ASR |

## 5. 集成形态（如果实测通过）

**独立常驻进程，不进主二进制**，三条理由：

1. onnxruntime 是 C++ 库，塞进 Rust 主二进制要交叉编译 ORT（坑深）；
2. `chuan.service` 有 `MemoryMax=1G`，模型 + ORT 常驻 300–500MB 会顶上限；
3. 模型加载要 1–3 秒，必须常驻，不能每次调用重载。

形态：`chuan-asr.service`（独立 unit，自己的 cgroup，`Nice=5` 避免抢 bot 的 CPU）加载一次模型，
监听 unix socket；chuan 在**宿主侧**（非沙盒）调用它，把 16k mono PCM 丢进去、拿回文本。

音频转换：TG 语音条是 OGG/OPUS，需要解码成 16k mono PCM——`ffmpeg`（~50MB）或 Rust 的
`symphonia` + `opus`。微信侧不需要（自带转写已确认）。

## 6. 成本

| 项 | 大小/代价 |
|---|---|
| sherpa-onnx（spacemit） | 21 MB |
| SenseVoice int8 模型 | ~150–250 MB |
| ffmpeg（若用） | ~50 MB |
| 常驻内存 | ~300–500 MB（独立 cgroup，不占主服务 1G） |
| 额外维护 | 1 个 unit + 模型更新流程 |

## 7. 未验证 ⚠️

| # | 项 | 怎么验 |
|---|---|---|
| ~~A1~~ | ~~K1 上的真实 RTF~~ | ✅ 实测：**4 线程 RTF 0.435**（2 线程 0.548、1 线程 0.786、8 线程 1.315 倒退） |
| A2 | int8 量化后中文准确率（专名、数字、方言） | 用 5–10 条真实语音比对 |
| ~~A3~~ | ~~线程数收益~~ | ✅ 实测：4 线程最优；8 线程明显倒退（内存带宽瓶颈） |
| A4 | 长语音（>30s）是否要切段 | 试一条 60s |
| A5 | 与 `MemoryMax=1G` 共存 | ✅ RSS ≈ 292 MB；仍建议独立 unit，但理由改成 **CPU 隔离**（`Nice=5`），不是内存 |

## 8. 结论

- **技术路径明确且已验证**：sherpa-onnx 的 spacemit 构建 + SenseVoice int8，CPU/RVV，无 NPU 依赖。
- **A1 结论：4 线程 RTF 0.435** → 10 秒语音约 4.3 秒，落在"可用"区间。
- **但准确率尚未验**（A2）：测试音频把"开放时间"识别成"放时间"，语种也判成粤语
  （`--sense-voice-language=zh` 没改变语种头）。必须用真实语音样本再评估。
- 微信侧**不需要**本地 ASR（自带转写已验证）；本地 ASR 的唯一动机是 **TG 语音**和**离线/隐私**。
  如果 TG 语音不是刚需，这件事可以先不做——云端 ASR 更便宜也更准。
