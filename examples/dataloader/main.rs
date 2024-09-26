use usls::{models::YOLO, Annotator, DataLoader, Nms, Options, Vision, YOLOTask, YOLOVersion};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::ERROR)
        .init();

    let options = Options::new()
        .with_cuda(0)
        .with_model("yolo/v8-m-dyn.onnx")?
        .with_yolo_version(YOLOVersion::V8)
        .with_yolo_task(YOLOTask::Detect)
        .with_i00((1, 1, 4).into())
        .with_i02((0, 640, 640).into())
        .with_i03((0, 640, 640).into())
        .with_confs(&[0.2]);

    let mut model = YOLO::new(options)?;

    // build dataloader
    let dl = DataLoader::new(
        // "images/bus.jpg",  // remote image
        // "../images", // image folder
        // "assets/detect.mp4",   // local video
        // "http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4", // remote video
        // "rtsp://admin:xyz@192.168.2.217:554/h265/ch1/",  // rtsp h264 stream
        // "./assets/bus.jpg", // local image
        "rtmp://172.24.82.44/live/livestream1"
    )?
    .with_batch(3)
    .build()?;

    // build annotator
    let annotator = Annotator::new()
        .with_bboxes_thickness(4)
        .with_saveout("YOLO-DataLoader");

    // run
    for (xs, _) in dl {
        let ys = model.forward(&xs, false)?;
        annotator.annotate(&xs, &ys);
        // Retrieve inference results
        for y in ys {
            // bboxes
            if let Some(bboxes) = y.bboxes() {
                for bbox in bboxes {
                    println!(
                        "Bbox: {}, {}, {}, {}, {}, [{}:{}]",
                        bbox.xmin(),
                        bbox.ymin(),
                        bbox.xmax(),
                        bbox.ymax(),
                        bbox.confidence(),
                        bbox.id(),
                        bbox.name().unwrap().as_str()
                    );
                }
            }
        }
    }

    // images -> video
    // DataLoader::is2v("runs/YOLO-DataLoader", &["runs", "is2v"], 25)?;

    Ok(())
}
