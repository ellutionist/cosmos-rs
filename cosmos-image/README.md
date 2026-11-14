# cosmos-image

Cosmos 的 Rust 原生静态图像插件，提供 PNG、JPEG、WebP 编解码、缩放裁剪、旋转翻转和像素平面读写。基于 [image](https://docs.rs/image/0.25.10/image/)，仅启用三个对应编解码器，不依赖 OpenCV、libjpeg、libpng 或 libwebp。

CLI 默认不启用图片；构建时传入 `--features image` 或 `--features full` 启用。Rust 宿主通过 `machine.add_module(cosmos_image::Image)?` 注入，Lua 使用 `require('cosmos.image')`。插件独立于 Network/Encoding；完整用法见 [image.lua](../examples/image.lua)，HTTP 下载、上传与处理组合见 [image_http.lua](../examples/image_http.lua)。

`cosmos-cli examples/image.lua /tmp` 可额外验证临时文件原子写回；省略父目录参数则只验证内存图像操作。单独构建可使用 `cargo build --release -p cosmos-cli --no-default-features --features image`。

## 创建与编解码

| 接口 | 语义 |
| --- | --- |
| `image.decode(bytes, options?)` | 根据内容识别 PNG/JPEG/WebP，返回图像句柄 |
| `image.new(width, height, color?)` | 创建填充图像；缺省透明黑色，color 为 `{r,g,b,a?}`，提供 color 时 a 默认 255 |
| `image.from_pixels(width, height, format, bytes)` | 从行优先的 `luma8` / `rgb8` / `rgba8` 连续像素创建图像，长度必须精确匹配 |
| `picture:info()` | `{width, height, pixel_format='rgba8', bytes}` |
| `picture:width()` / `:height()` | 读取尺寸 |
| `picture:encode(format, options?)` | 返回编码字节；format 为 `png` / `jpeg`（或 `jpg`）/ `webp` |

内部统一为 RGBA8。灰度/RGB 输入补齐通道，16 位 PNG 转为 8 位；不保留原编码、元数据、ICC 配置或动画。动画输入仅读取默认图像/第一帧，不提供帧序列接口。PNG 与 WebP 输出无损，WebP 编码仅支持 lossless，不提供质量旋钮。

解码 options 的 `max_width` / `max_height` / `max_pixels` 可在硬上限内进一步收紧限制，分配像素缓冲前检查。`auto_orient=true` 按可用 EXIF 方向转换；缺省 false，保留原始存储方向。方向变换后的尺寸也必须满足指定限制。

JPEG 的 `quality` 为 1–100 的整数，默认 90。JPEG 不支持 alpha，默认直接丢弃 alpha，行为与原后端 `to_rgb8()` 一致；指定 `background={r,g,b}` 可先合成为不透明背景。background 的 a 如果给出，必须是 255。PNG/WebP 不接受 quality 或 background 参数。

## 变换与像素

| 接口 | 语义 |
| --- | --- |
| `picture:resize(width, height, filter?)` | 精确目标尺寸；默认 lanczos3 |
| `picture:thumbnail(width, height, upscale?)` | 等比适配边界，默认不放大；upscale=true 时允许放大，使用原生快速整数采样 |
| `picture:crop(x, y, width, height)` | 严格裁剪矩形，越界返回错误，不静默截断 |
| `picture:rotate(degrees)` | 顺时针 0 / 90 / 180 / 270 度 |
| `picture:flip(axis)` | horizontal / vertical |
| `picture:pixel(x, y)` | 返回 `{r,g,b,a}`，每个通道为 0–255 |
| `picture:pixels(format?)` | 返回连续像素字节，默认 rgba8，也支持 rgb8 / luma8 |
| `picture:with_pixels(x, y, width, height, format, bytes)` | 批量替换矩形像素，返回新图像；输入格式同 from_pixels |

坐标从左上角 0 开始，像素行优先存储，所有尺寸/坐标/通道均要求范围内整数。resize 的 filter 可选 `nearest` / `triangle` / `catmull_rom` / `gaussian` / `lanczos3`。灰度转换及采样使用原生 image 算法，直接处理通道值，不自动进行色彩管理或 alpha 预乘；rgb8/luma8 导出会丢弃 alpha。

图像是不可变共享值。所有变换返回新句柄，原图与并发操作互不修改；引用复制只共享像素缓冲，实际生成新图像时才分配。with_pixels 一次复制底图后应用整个矩形，适合批量更新，避免逐像素构造新图像。文件访问、下载策略、海报比例和水印检测由应用组合 FS、HTTP 与这些基础操作。

## 异步与资源

创建、解码、编码、变换及像素平面转换均在 Tokio 阻塞工作线程执行；尺寸、info 和单点 pixel 查询同步执行。异步接口应在 Machine 驱动的可挂起 Lua 协程中调用，require 初始化阶段可保存参数，进入执行阶段再创建图像。

- 单边最多 16000，最多 33554432 像素，每幅 RGBA8 缓冲最多 128 MiB；禁止零尺寸。
- 编码输入和输出各最多 64 MiB，原始像素输入/输出最多 128 MiB。编码器写出时检查输出上限；大像素平面写文件可按 FS 的分块限制拆分。
- 解码器配置 128 MiB 分配预算，同时自行检查头部尺寸、像素数及解码字节数。编解码器内部的额外分配沿用原生库的能力，此预算不是进程 RSS 的硬上限。
- resize 在分配前检查浮点中间图最多 256 MiB、源/目标/中间像素缓冲的合计估算最多 512 MiB。长宽比交叉可能在输入、输出都很小时触发这一限制。
- 所有 Machine/Runtime 共享最多 2 个原生图像作业、16 个已接纳作业（包括运行中）；队列满返回 `nil, "image worker queue is full"`。应用持有的图像数量和内存不计入作业预算，由应用自行管理。

大 Lua 字节串在等待期间由独立锚点保活，获取额度后才准备拥有型 Rust 数据；工作线程不接收 Lua 引用。取消或 VM 销毁可撤销未开始的作业，已运行的计算不能强制中断，会继续占用额度直到结束，随后释放输入、输出和中间缓冲。失败或取消不改变原图。

解码、参数范围、格式、内存预算和作业错误返回 `nil, err`；参数类型错误沿用 Lua 桥接错误。释放最后一个图像句柄时回收缓冲，无需显式 close。
