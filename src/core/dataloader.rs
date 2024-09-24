use crate::core::avio_writing;
use crate::{
    core::avio, string_now, Dir, Hub, Location, MediaType, CHECK_MARK, CROSS_MARK,
};
use anyhow::{anyhow, Context, Error, Result};
use image::DynamicImage;
use indicatif::{ProgressBar, ProgressStyle};
use rsmpeg::{
    avcodec::AVCodecContext,
    avformat::AVFormatContextInput,
    avutil::{AVFrame, AVFrameWithImage, AVImage, AVRational},
    ffi::{self, AVFormatContext, AVInputFormat},
    swscale::SwsContext,
};
use std::collections::VecDeque;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::AtomicI64;
use std::sync::mpsc;


type TempReturnType = (Vec<DynamicImage>, Vec<PathBuf>);

pub struct DataLoaderIterator {
    receiver: mpsc::Receiver<TempReturnType>,
    progress_bar: Option<ProgressBar>,
}

impl Iterator for DataLoaderIterator {
    type Item = TempReturnType;

    fn next(&mut self) -> Option<Self::Item> {
        match &self.progress_bar {
            None => self.receiver.recv().ok(),
            Some(progress_bar) => match self.receiver.recv().ok() {
                Some(item) => {
                    progress_bar.inc(1);
                    Some(item)
                }
                None => {
                    progress_bar.set_prefix("    Iterated");
                    progress_bar.set_style(
                        indicatif::ProgressStyle::with_template(crate::PROGRESS_BAR_STYLE_FINISH_2)
                            .unwrap(),
                    );
                    progress_bar.finish();
                    None
                }
            },
        }
    }
}

impl IntoIterator for DataLoader {
    type Item = TempReturnType;
    type IntoIter = DataLoaderIterator;

    fn into_iter(self) -> Self::IntoIter {
        let progress_bar = if self.with_pb {
            crate::build_progress_bar(
                self.nf / self.batch_size as u64,
                "   Iterating",
                Some(&format!("{:?}", self.media_type)),
                crate::PROGRESS_BAR_STYLE_CYAN_2,
            )
                .ok()
        } else {
            None
        };

        DataLoaderIterator {
            receiver: self.receiver,
            progress_bar,
        }
    }
}

/// A structure designed to load and manage image, video, or stream data.
/// It handles local file paths, remote URLs, and live streams, supporting both batch processing
/// and optional progress bar display. The structure also supports video decoding through
/// `video_rs` for video and stream data.
pub struct DataLoader {
    /// Queue of paths for images.
    paths: Option<VecDeque<PathBuf>>,

    /// Media type of the source (image, video, stream, etc.).
    media_type: MediaType,

    /// Batch size for iteration, determining how many files are processed at once.
    batch_size: usize,

    /// Buffer size for the channel, used to manage the buffer between producer and consumer.
    bound: usize,

    /// Receiver for processed data.
    receiver: mpsc::Receiver<TempReturnType>,

    /// Video decoder for handling video or stream data.
    decoder: Option<avio::Decoder>,

    /// Number of images or frames; `u64::MAX` is used for live streams (indicating no limit).
    nf: u64,

    /// Flag indicating whether to display a progress bar.
    with_pb: bool,
}

impl DataLoader {
    pub fn new(source: &str) -> Result<Self> {
        let span = tracing::span!(tracing::Level::INFO, "DataLoader-new");
        let _guard = span.enter();

        // Number of frames or stream
        let mut nf = 0;

        // paths & media_type
        let source_path = Path::new(source);
        let (paths, media_type) = match source_path.exists() {
            false => {
                // remote
                nf = 1;
                (
                    Some(VecDeque::from([source_path.to_path_buf()])),
                    MediaType::from_url(source),
                )
            }
            true => {
                // local
                if source_path.is_file() {
                    nf = 1;
                    (
                        Some(VecDeque::from([source_path.to_path_buf()])),
                        MediaType::from_path(source_path),
                    )
                } else if source_path.is_dir() {
                    let paths_sorted = Self::load_from_folder(source_path)?;
                    nf = paths_sorted.len() as _;
                    (
                        Some(VecDeque::from(paths_sorted)),
                        MediaType::Image(Location::Local),
                    )
                } else {
                    (None, MediaType::Unknown)
                }
            }
        };

        if let MediaType::Unknown = media_type {
            anyhow::bail!("Could not locate the source path: {:?}", source_path);
        }

        // decoder
        let decoder = avio::Decoder::new(source)?;

        // summary
        tracing::info!("{} Found {:?} x{}", CHECK_MARK, media_type, nf,);

        Ok(DataLoader {
            paths,
            media_type,
            bound: 50,
            receiver: mpsc::sync_channel(1).1,
            batch_size: 1,
            decoder: Some(decoder),
            nf,
            with_pb: true,
        })
    }

    pub fn with_bound(mut self, x: usize) -> Self {
        self.bound = x;
        self
    }

    pub fn with_batch(mut self, x: usize) -> Self {
        self.batch_size = x;
        self
    }

    pub fn with_progress_bar(mut self, x: bool) -> Self {
        self.with_pb = x;
        self
    }

    pub fn build(mut self) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<TempReturnType>(self.bound);
        self.receiver = receiver;
        let batch_size = self.batch_size;
        let data = self.paths.take().unwrap_or_default();
        let media_type = self.media_type.clone();
        let decoder = self.decoder.take();

        // Spawn the producer thread
        std::thread::spawn(move || {
            DataLoader::producer_thread(sender, data, batch_size, media_type, decoder);
        });

        Ok(self)
    }

    fn producer_thread(
        sender: mpsc::SyncSender<TempReturnType>,
        mut data: VecDeque<PathBuf>,
        batch_size: usize,
        media_type: MediaType,
        mut decoder: Option<avio::Decoder>,
    ) {
        let span = tracing::span!(tracing::Level::INFO, "DataLoader-producer-thread");
        let _guard = span.enter();
        let mut yis: Vec<DynamicImage> = Vec::with_capacity(batch_size);
        let mut yps: Vec<PathBuf> = Vec::with_capacity(batch_size);

        match media_type {
            MediaType::Image(_) => {
                while let Some(path) = data.pop_front() {
                    match Self::try_read(&path) {
                        Err(err) => {
                            tracing::warn!("{} {:?} | {:?}", CROSS_MARK, path, err);
                            continue;
                        }
                        Ok(img) => {
                            yis.push(img);
                            yps.push(path);
                        }
                    }
                    if yis.len() == batch_size
                        && sender
                        .send((std::mem::take(&mut yis), std::mem::take(&mut yps)))
                        .is_err()
                    {
                        break;
                    }
                }
            }
            MediaType::Video(_) | MediaType::Stream => {
                if let Some(decoder) = decoder.as_mut() {
                    match decoder.decode_frames() {
                        Ok(images) => {
                            for img in images {
                                yis.push(img);
                                // TODO: Adjust based on timestamp or other identifiers
                                yps.push(PathBuf::new());

                                if yis.len() == batch_size &&
                                    sender.send((std::mem::take(&mut yis), std::mem::take(&mut yps))).is_err()
                                {
                                    break;
                                }
                            }
                        }
                        Err(err) => {
                            tracing::warn!("Error decoding frames: {:?}", err);
                        }
                    }
                }
            }
            _ => todo!(),
        }

        // Deal with remaining data
        if !yis.is_empty() && sender.send((yis, yps)).is_err() {
            tracing::info!("Receiver dropped, stopping production");
        }
    }

    pub fn load_from_folder<P: AsRef<std::path::Path>>(path: P) -> Result<Vec<std::path::PathBuf>> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(path)?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let path = entry.path();
                if path.is_file() {
                    Some(path)
                } else {
                    None
                }
            })
            .collect();

        paths.sort_by(|a, b| {
            let a_name = a.file_name().and_then(|s| s.to_str());
            let b_name = b.file_name().and_then(|s| s.to_str());

            match (a_name, b_name) {
                (Some(a_str), Some(b_str)) => natord::compare(a_str, b_str),
                _ => std::cmp::Ordering::Equal,
            }
        });

        Ok(paths)
    }

    pub fn try_read<P: AsRef<Path>>(path: P) -> Result<DynamicImage> {
        let mut path = path.as_ref().to_path_buf();

        // try to fetch from hub or local cache
        if !path.exists() {
            let p = Hub::new()?.fetch(path.to_str().unwrap())?.commit()?;
            path = PathBuf::from(&p);
        }
        let img = Self::read_into_rgb8(path)?;
        Ok(DynamicImage::from(img))
    }

    fn read_into_rgb8<P: AsRef<Path>>(path: P) -> Result<image::RgbImage> {
        let path = path.as_ref();
        let img = image::ImageReader::open(path)
            .map_err(|err| {
                anyhow!(
                    "Failed to open image at {:?}. Error: {:?}",
                    path.display(),
                    err
                )
            })?
            .with_guessed_format()
            .map_err(|err| {
                anyhow!(
                    "Failed to make a format guess based on the content: {:?}. Error: {:?}",
                    path.display(),
                    err
                )
            })?
            .decode()
            .map_err(|err| {
                anyhow!(
                    "Failed to decode image at {:?}. Error: {:?}",
                    path.display(),
                    err
                )
            })?
            .into_rgb8();
        Ok(img)
    }

    /// Convert images into a video
    pub fn is2v<P: AsRef<Path>>(source: P, subs: &[&str], fps: usize) -> Result<()> {
        let paths = Self::load_from_folder(source.as_ref())?;
        if paths.is_empty() {
            anyhow::bail!("No images found.");
        }

        let saveout = Dir::Currnet
            .raw_path_with_subs(subs)?
            .join(format!("{}.mp4", string_now("-")));
        let saveout = saveout.to_string_lossy().to_string();

        let pb = crate::build_progress_bar(
            paths.len() as u64,
            "  Converting",
            Some(&format!("{:?}", MediaType::Video(Location::Local))),
            crate::PROGRESS_BAR_STYLE_CYAN_2,
        )?;

        let image0 = Self::read_into_rgb8(paths[0].clone())?;
        let (width, height) = image0.dimensions();
        let (mut output_format_context, mut encode_context) = avio::open_output_file_custom(
            CString::new(saveout.clone()).unwrap().as_c_str(),
            width as i32,
            height as i32,
            AVRational { num: 16, den: 9 },
            AVRational { num: fps as i32, den: 1 },
            1,
        )?;

        // 定义输出格式
        let src_format = ffi::AV_PIX_FMT_RGB24;
        let dst_format = ffi::AV_PIX_FMT_YUV420P;

        let mut frame_pts = AtomicI64::new(1);
        // loop
        for path in paths {
            pb.inc(1);

            // 1. 读取图片 -> RGB 数据
            let rgb_img = Self::read_into_rgb8(path)?;
            let (w, h) = rgb_img.dimensions();

            // 2. 创建源 AVFrame，并分配缓冲区
            let mut src_frame = AVFrame::new();
            src_frame.set_width(w as i32);
            src_frame.set_height(h as i32);
            src_frame.set_format(src_format);
            src_frame.set_pts(frame_pts.fetch_add(1, std::sync::atomic::Ordering::SeqCst));
            src_frame.alloc_buffer()?;

            // 3. 将 image 的 RGB 数据拷贝到 src_frame 中
            let rgb_data = rgb_img.into_raw();
            let data_arr = ndarray::Array3::from_shape_vec((h as usize, w as usize, 3), rgb_data)
                .expect("Failed to create ndarray from raw image data");
            unsafe {
                let buffer_slice = std::slice::from_raw_parts_mut(src_frame.data[0], data_arr.len());
                buffer_slice.copy_from_slice(data_arr.as_slice().expect("Failed to get ndarray::Array3 as slice"));
            }

            // 4. 创建目标 AVFrame (YUV420P 格式)
            let mut dst_frame = AVFrame::new();
            dst_frame.set_width(w as i32);
            dst_frame.set_height(h as i32);
            dst_frame.set_format(dst_format);
            dst_frame.alloc_buffer()?;

            // 5. 创建 sws_context
            let mut sws_context = SwsContext::get_context(
                w as i32,
                h as i32,
                src_format,
                w as i32,
                h as i32,
                dst_format,
                ffi::SWS_BILINEAR | ffi::SWS_PRINT_INFO,
                None,
                None,
                None,
            ).context("Failed to create SwsContext")?;


            // 6. 执行 sws_context.scale 转换
            unsafe {
                let src_stride = &src_frame.linesize[0] as *const i32; // 源图像的每行步幅
                let dst_stride = &dst_frame.linesize[0] as *const i32; // 目标图像的每行步幅

                // 使用 scale 函数进行图像转换 (RGB -> YUV420P)
                sws_context.scale(
                    src_frame.data.as_ptr() as *const *const u8,  // 源图像数据
                    src_stride,                                   // 源图像每行步幅
                    0,                                  // 开始处理的行
                    h as i32,                                     // 要处理的行数
                    dst_frame.data.as_ptr() as *const *mut u8,    // 目标图像数据
                    dst_stride,                                   // 目标图像每行步幅
                )?;
                // let _ = sws_context.scale_frame(&src_frame, w as i32, h as i32, &mut dst_frame)?;
            }

            // pts
            dst_frame.set_pts(src_frame.pts);

            avio_writing::encode_write_frame(
                Some(&dst_frame),
                &mut encode_context,
                &mut output_format_context,
                0,
            )?;

            println!("Image conversion successful!");
        }

        // Flush the encoder by pushing EOF frame to encode_context.
        avio_writing::flush_encoder(&mut encode_context, &mut output_format_context, 0)?;
        output_format_context.write_trailer()?;

        // update
        pb.set_prefix("   Converted");
        pb.set_message(saveout.clone());
        pb.set_style(ProgressStyle::with_template(
            crate::PROGRESS_BAR_STYLE_FINISH_4,
        )?);
        pb.finish();

        Ok(())
    }
}
