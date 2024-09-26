use anyhow::{anyhow, Context, Result};
use image::{DynamicImage, ImageBuffer, Rgb, RgbImage};
use rsmpeg::avcodec::{AVCodec, AVCodecContext, AVPacket};
use rsmpeg::avformat::{
    AVFormatContextInput, AVFormatContextOutput, AVIOContextContainer, AVIOContextCustom,
};
use rsmpeg::avutil::{self, AVFrame, AVMem, AVRational};
use rsmpeg::error::RsmpegError;
use rsmpeg::ffi;
use rsmpeg::swscale::SwsContext;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{BufWriter, Seek};
use std::io::{SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// multimedia file input decoding
pub struct Decoder {
    stream_index: usize,
    codec_context: AVCodecContext,
    format_context: AVFormatContextInput,
    current_packet: Option<AVPacket>,
}

impl Decoder {
    pub fn new(source: &str) -> Result<Self> {
        let (stream_idx, input_format_context, decode_context) =
            open_input_file(CString::new(source).unwrap().as_c_str())?;
        Ok(Decoder {
            stream_index: stream_idx,
            codec_context: decode_context,
            format_context: input_format_context,
            current_packet: None,
        })
    }

    pub fn get_framerate(&self) -> Result<u64> {
        let stream = &self.format_context.streams()[self.stream_index];
        let frame_rate = stream.guess_framerate().unwrap();
        Ok((frame_rate.num as f64 / frame_rate.den as f64) as u64)
    }

    pub fn decode_iter(&mut self) -> impl Iterator<Item=Result<(i64, DynamicImage), anyhow::Error>> + '_ {
        std::iter::from_fn(move || {
            match self.decode_next() {
                Ok(Some(frame)) => Some(Ok(frame)),
                Ok(None) => None,
                Err(e) => Some(Err(e)),
            }
        })
    }

    fn frame_to_dynamic_image(&self, frame: &AVFrame) -> Result<DynamicImage, anyhow::Error> {
        let width = frame.width as u32;
        let height = frame.height as u32;
        let buffer: Vec<u8> = unsafe {
            let data_ptr = frame.data[0];
            let size = (frame.linesize[0] * frame.height) as usize;
            std::slice::from_raw_parts(data_ptr as *const u8, size).to_vec()
        };
        let img = image::ImageBuffer::from_raw(width, height, buffer)
            .ok_or_else(|| anyhow!("Failed to create image buffer"))?;
        Ok(DynamicImage::ImageRgb8(img))
    }

    fn decode_next(&mut self) -> Result<Option<(i64, DynamicImage)>, anyhow::Error> {
        loop {
            // 在这里实现解码逻辑
            if self.current_packet.is_none() {
                while let Some(packet) = self.format_context.read_packet()? {
                    if packet.stream_index as usize == self.stream_index {
                        self.current_packet = Some(packet);
                        break;
                    }
                }
            }

            println!("decode_next current_packet: {}", self.current_packet.is_some());

            let mut sws_context = SwsContext::get_context(
                self.codec_context.width,
                self.codec_context.height,
                ffi::AV_PIX_FMT_YUV420P,
                self.codec_context.width,
                self.codec_context.height,
                ffi::AV_PIX_FMT_RGB24,
                ffi::SWS_BILINEAR | ffi::SWS_PRINT_INFO,
                None,
                None,
                None,
            ).context("Failed to create SwsContext")?;

            if let Some(packet) = self.current_packet.take() {
                println!("packet pts: {}, dts: {}, duration: {}, size: {}, stream_index: {}",
                         packet.pts, packet.dts, packet.duration, packet.size, packet.stream_index);

                self.codec_context.send_packet(Some(&packet))
                    .context("Failed to send packet to codec context")?;

                while let Ok(frame) = self.codec_context.receive_frame() {
                    // 注意这里的 frame 编码格式为 YUV420P，需要转换为 RGB24
                    let mut rgb_frame = AVFrame::new();
                    rgb_frame.set_format(ffi::AV_PIX_FMT_RGB24);
                    rgb_frame.set_width(self.codec_context.width);
                    rgb_frame.set_height(self.codec_context.height);
                    rgb_frame.set_time_base(frame.time_base);
                    rgb_frame.set_pict_type(frame.pict_type);
                    rgb_frame.set_pts(frame.pts);
                    rgb_frame.alloc_buffer()?;

                    sws_context.scale_frame(
                        &frame,
                        0,
                        self.codec_context.height,
                        &mut rgb_frame,
                    )?;

                    println!("convert frame from yuv420p to rgb24 pts: {}, time_base: {:?}", rgb_frame.pts, rgb_frame.time_base);
                    let img = self.frame_to_dynamic_image(&rgb_frame)?;
                    return Ok(Some((frame.pts, img)));
                }
            } else {
                println!("No packet received for stream_index: {}", self.stream_index);
                break;
            }
        }

        Ok(None)
    }
}

/// Get `video_stream_index`, `input_format_context`, `decode_context`.
pub fn open_input_file(
    filename: &CStr,
) -> anyhow::Result<(usize, AVFormatContextInput, AVCodecContext)> {
    let mut input_format_context = AVFormatContextInput::open(filename, None, &mut None)?;
    input_format_context.dump(0, filename)?;

    let (video_index, decoder) = input_format_context
        .find_best_stream(ffi::AVMEDIA_TYPE_VIDEO)
        .context("Failed to select a video stream")?
        .context("No video stream")?;

    println!("open_input_file: video_index: {}, decoder: {:?}", video_index, decoder.name().to_str()?);

    let decode_context = {
        let input_stream = &input_format_context.streams()[video_index];

        let mut decode_context = AVCodecContext::new(&decoder);
        decode_context.apply_codecpar(&input_stream.codecpar())?;
        if let Some(framerate) = input_stream.guess_framerate() {
            decode_context.set_framerate(framerate);
        }
        decode_context.open(None)?;
        decode_context
    };

    Ok((video_index, input_format_context, decode_context))
}

/// Return output_format_context and encode_context
pub fn open_output_file(
    filename: &CStr,
    decode_context: &AVCodecContext,
) -> anyhow::Result<(AVFormatContextOutput, AVCodecContext)> {
    let buffer = Arc::new(Mutex::new(File::create(filename.to_str()?)?));
    let buffer1 = buffer.clone();

    // Custom IO Context
    let io_context = AVIOContextCustom::alloc_context(
        AVMem::new(4096),
        true,
        vec![],
        None,
        Some(Box::new(move |_: &mut Vec<u8>, buf: &[u8]| {
            let mut buffer = buffer1.lock().unwrap();
            buffer.write_all(buf).unwrap();
            buf.len() as _
        })),
        Some(Box::new(
            move |_: &mut Vec<u8>, offset: i64, whence: i32| {
                println!("offset: {}, whence: {}", offset, whence);
                let mut buffer = match buffer.lock() {
                    Ok(x) => x,
                    Err(_) => return -1,
                };
                let mut seek_ = |offset: i64, whence: i32| -> anyhow::Result<i64> {
                    Ok(match whence {
                        libc::SEEK_CUR => buffer.seek(SeekFrom::Current(offset))?,
                        libc::SEEK_SET => buffer.seek(SeekFrom::Start(offset as u64))?,
                        libc::SEEK_END => buffer.seek(SeekFrom::End(offset))?,
                        _ => return Err(anyhow!("Unsupported whence")),
                    } as i64)
                };
                seek_(offset, whence).unwrap_or(-1)
            },
        )),
    );

    let mut output_format_context =
        AVFormatContextOutput::create(filename, Some(AVIOContextContainer::Custom(io_context)))?;

    let encoder = AVCodec::find_encoder(ffi::AV_CODEC_ID_H264)
        .with_context(|| anyhow!("encoder({}) not found.", ffi::AV_CODEC_ID_H264))?;

    let mut encode_context = AVCodecContext::new(&encoder);
    encode_context.set_height(decode_context.height);
    encode_context.set_width(decode_context.width);
    encode_context.set_sample_aspect_ratio(decode_context.sample_aspect_ratio);
    encode_context.set_pix_fmt(if let Some(pix_fmts) = encoder.pix_fmts() {
        pix_fmts[0]
    } else {
        decode_context.pix_fmt
    });
    encode_context.set_time_base(avutil::av_inv_q(avutil::av_mul_q(
        decode_context.framerate,
        AVRational {
            num: decode_context.ticks_per_frame,
            den: 1,
        },
    )));

    // Some formats want stream headers to be separate.
    if output_format_context.oformat().flags & ffi::AVFMT_GLOBALHEADER as i32 != 0 {
        encode_context.set_flags(encode_context.flags | ffi::AV_CODEC_FLAG_GLOBAL_HEADER as i32);
    }

    encode_context.open(None)?;

    {
        let mut out_stream = output_format_context.new_stream();
        out_stream.set_codecpar(encode_context.extract_codecpar());
        out_stream.set_time_base(encode_context.time_base);
    }

    output_format_context.dump(0, filename)?;
    output_format_context.write_header(&mut None)?;

    Ok((output_format_context, encode_context))
}

/// filename (&CStr): 这是一个指向 C 字符串的指针，表示输出视频文件的名称
/// width (i32): 输出视频的宽度，以像素为单位。
/// height (i32): 输出视频的高度，以像素为单位。
/// ratio (AVRational): 输出视频的纵横比，表示为一个分数
/// framerate (AVRational): 输出视频的帧率，表示为每秒帧数 (fps)
/// ticks_per_frame (i32): 每帧的时间戳增量。
pub fn open_output_file_custom(
    filename: &CStr,
    width: i32,
    height: i32,
    ratio: AVRational,
    framerate: AVRational,
    ticks_per_frame: i32,
) -> anyhow::Result<(AVFormatContextOutput, AVCodecContext)> {
    let buffer = Arc::new(Mutex::new(File::create(filename.to_str()?)?));
    let buffer1 = buffer.clone();

    // Custom IO Context
    let io_context = AVIOContextCustom::alloc_context(
        AVMem::new(4096),
        true,
        vec![],
        None,
        Some(Box::new(move |_: &mut Vec<u8>, buf: &[u8]| {
            let mut buffer = buffer1.lock().unwrap();
            buffer.write_all(buf).unwrap();
            buf.len() as _
        })),
        Some(Box::new(
            move |_: &mut Vec<u8>, offset: i64, whence: i32| {
                println!("offset: {}, whence: {}", offset, whence);
                let mut buffer = match buffer.lock() {
                    Ok(x) => x,
                    Err(_) => return -1,
                };
                let mut seek_ = |offset: i64, whence: i32| -> anyhow::Result<i64> {
                    Ok(match whence {
                        libc::SEEK_CUR => buffer.seek(SeekFrom::Current(offset))?,
                        libc::SEEK_SET => buffer.seek(SeekFrom::Start(offset as u64))?,
                        libc::SEEK_END => buffer.seek(SeekFrom::End(offset))?,
                        _ => return Err(anyhow!("Unsupported whence")),
                    } as i64)
                };
                seek_(offset, whence).unwrap_or(-1)
            },
        )),
    );

    let mut output_format_context =
        AVFormatContextOutput::create(filename, Some(AVIOContextContainer::Custom(io_context)))?;

    let encoder = AVCodec::find_encoder(ffi::AV_CODEC_ID_H264)
        .with_context(|| anyhow!("encoder({}) not found.", ffi::AV_CODEC_ID_H264))?;

    let mut encode_context = AVCodecContext::new(&encoder);
    encode_context.set_width(width);
    encode_context.set_height(height);
    encode_context.set_sample_aspect_ratio(ratio);
    encode_context.set_pix_fmt(if let Some(pix_fmts) = encoder.pix_fmts() {
        pix_fmts[0]
    } else {
        ffi::AV_PIX_FMT_YUV420P
    });

    encode_context.set_time_base(avutil::av_inv_q(avutil::av_mul_q(
        framerate,
        AVRational {
            num: ticks_per_frame,
            den: 1,
        },
    )));

    // Some formats want stream headers to be separate.
    if output_format_context.oformat().flags & ffi::AVFMT_GLOBALHEADER as i32 != 0 {
        encode_context.set_flags(encode_context.flags | ffi::AV_CODEC_FLAG_GLOBAL_HEADER as i32);
    }

    encode_context.open(None)?;

    {
        let mut out_stream = output_format_context.new_stream();
        out_stream.set_codecpar(encode_context.extract_codecpar());
        out_stream.set_time_base(encode_context.time_base);
    }

    output_format_context.dump(0, filename)?;
    output_format_context.write_header(&mut None)?;

    Ok((output_format_context, encode_context))
}

/// Save a `AVFrame` as *colorful* pgm file.
pub fn pgm_save(frame: &AVFrame, filename: &str) -> anyhow::Result<()> {
    // Here we only capture the first layer of frame.
    let data = frame.data[0];
    let linesize = frame.linesize[0] as usize;

    let width = frame.width as usize;
    let height = frame.height as usize;

    let buffer = unsafe { std::slice::from_raw_parts(data, height * linesize * 3) };

    // Create pgm file
    let mut pgm_file = File::create(filename)?;

    // Write pgm header(P6 means colorful)
    pgm_file.write_all(&format!("P6\n{} {}\n{}\n", width, height, 255).into_bytes())?;

    // Write pgm data
    for i in 0..height {
        // Here the linesize is bigger than width * 3.
        pgm_file.write_all(&buffer[i * linesize..i * linesize + width * 3])?;
    }
    Ok(())
}

// 将 RgbImage 转换为 AVFrame
pub fn rgb_image_to_avframe_yuv420p(image: &RgbImage, frame_pts: i64) -> AVFrame {
    let (width, height) = image.dimensions();

    // 定义输出格式
    let src_format = ffi::AV_PIX_FMT_RGB24;
    let dst_format = ffi::AV_PIX_FMT_YUV420P;

    // 2. 创建源 AVFrame，并分配缓冲区
    let mut src_frame = AVFrame::new();
    src_frame.set_width(width as i32);
    src_frame.set_height(height as i32);
    src_frame.set_format(src_format);
    src_frame.set_pts(frame_pts);
    src_frame.alloc_buffer().unwrap();

    // 3. 将 image 的 RGB 数据拷贝到 src_frame 中
    // let data_arr = ndarray::Array3::from_shape_vec((height as usize, width as usize, 3), image.into_raw())
    //     .expect("Failed to create ndarray from raw image data");
    unsafe {
        let rgb_data = image.as_raw();
        let buffer_slice = std::slice::from_raw_parts_mut(src_frame.data[0], rgb_data.len());
        buffer_slice.copy_from_slice(rgb_data);
    }

    // 4. 创建目标 AVFrame (YUV420P 格式)
    let mut dst_frame = AVFrame::new();
    dst_frame.set_width(width as i32);
    dst_frame.set_height(height as i32);
    dst_frame.set_format(dst_format);
    dst_frame.alloc_buffer().unwrap();

    // 5. 创建 sws_context
    let mut sws_context = SwsContext::get_context(
        width as i32,
        height as i32,
        src_format,
        width as i32,
        height as i32,
        dst_format,
        ffi::SWS_BILINEAR | ffi::SWS_PRINT_INFO,
        None,
        None,
        None,
    ).context("Failed to create SwsContext").unwrap();

    // 6. 执行 sws_context.scale 转换
    unsafe {
        let src_stride = &src_frame.linesize[0] as *const i32; // 源图像的每行步幅
        let dst_stride = &dst_frame.linesize[0] as *const i32; // 目标图像的每行步幅

        // 使用 scale 函数进行图像转换 (RGB -> YUV420P)
        let _ = sws_context.scale(
            src_frame.data.as_ptr() as *const *const u8,  // 源图像数据
            src_stride,                                   // 源图像每行步幅
            0,                                  // 开始处理的行
            height as i32,                                // 要处理的行数
            dst_frame.data.as_ptr() as *const *mut u8,    // 目标图像数据
            dst_stride,                                   // 目标图像每行步幅
        ).unwrap();

        // TODO: error handling
        // let _ = sws_context.scale_frame(&src_frame, w as i32, h as i32, &mut dst_frame)?;
    }

    // pts
    dst_frame.set_pts(src_frame.pts);

    dst_frame
}

pub fn save_avframe_yuv420p(
    frame: &AVFrame,
    width: i32,
    height: i32,
    output_file_name: &str,
) -> Result<()> {
    // 创建转换上下文，将帧转换为 RGB 格式
    let mut sws_ctx = SwsContext::get_context(
        width,
        height,
        ffi::AV_PIX_FMT_YUV420P, // 假设输入是 YUV420P
        width,
        height,
        ffi::AV_PIX_FMT_RGB24,
        ffi::SWS_BILINEAR | ffi::SWS_PRINT_INFO,
        None,
        None,
        None,
    ).ok_or(RsmpegError::Unknown)?;

    // 创建目标缓冲区
    let mut rgb_frame = AVFrame::new();
    rgb_frame.set_format(ffi::AV_PIX_FMT_RGB24);
    rgb_frame.set_width(width);
    rgb_frame.set_height(height);
    rgb_frame.alloc_buffer()?;

    // 设置边距
    let src_slice = frame.data.as_ptr() as *const *const u8;
    let src_stride = frame.linesize.as_ptr();

    let dest_slice = rgb_frame.data.as_ptr() as *const *mut u8;
    let dest_stride = rgb_frame.linesize.as_ptr();

    // 转换为 RGB 格式
    unsafe {
        let _ = sws_ctx.scale(src_slice, src_stride, 0, height, dest_slice, dest_stride);
    }

    // 从 RGB 数据创建图像缓冲区
    let data = unsafe {
        std::slice::from_raw_parts(rgb_frame.data[0], (width * height * 3) as usize)
    };

    let buffer: ImageBuffer<Rgb<u8>, _> =
        ImageBuffer::from_raw(width as u32, height as u32, data).ok_or(RsmpegError::Unknown)?;

    // 确定输出格式并写入文件
    let path = Path::new(output_file_name);
    let extension = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or(RsmpegError::Unknown)?;

    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);

    match extension.to_lowercase().as_str() {
        "png" => buffer.write_to(&mut writer, image::ImageFormat::Png)?,
        "jpg" | "jpeg" => buffer.write_to(&mut writer, image::ImageFormat::Jpeg)?,
        _ => return Err(RsmpegError::Unknown.into()),
    }

    println!("Image saved to {}", output_file_name);

    Ok(())
}

// pub fn save_avframe_rgb24(frame: &AVFrame, output_file_name: &str) -> Result<(), Box<dyn std::error::Error>> {
//
// }
