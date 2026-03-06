use anyhow::{Result, anyhow};
use crossbeam_channel::{Receiver, Sender};
use gstreamer::{ClockTime, Registry};
use gstreamer::glib::object::{Cast, ObjectExt};
use gstreamer::{Buffer, prelude::*};
use gstreamer_app::ffi::GstAppLeakyType;
use gstreamer_app::gst_base::ffi::GstDataQueue;
use gtk::prelude::*;
use gtk::{Application, ApplicationWindow, Box as GtkBox, Label, Orientation, Overlay};
use laminar::{Packet, Socket, SocketEvent};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll::{self, Pending, Ready};
use std::thread;
use std::time::Duration;
use stick::{Controller, Event, Listener};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

struct VideoStats {
    fps: f64,
    bitrate: u64,
    ping: u32,
}

mod messages;
use crate::messages::Message;

type Exit = usize;

struct State {
    sender: Sender<Packet>,
    listener: Listener,
    controllers: Vec<Controller>,
    rumble: (f32, f32),
    server: SocketAddr,
    dir: Arc<Mutex<f64>>,
    power: Arc<Mutex<f64>>,
    power_max: i8,
}

fn to_motor_vector(steer: f64, power: f64) -> (i16, i16) {
    ((steer * -14.) as i16 + 91, (power * -14.) as i16 + 90)
}

impl State {
    fn connect(&mut self, controller: Controller) -> Poll<Exit> {
        println!(
            "Connected p{}, id: {:016X}, name: {}",
            self.controllers.len() + 1,
            controller.id(),
            controller.name(),
        );

        self.controllers.push(controller);
        Pending
    }

    fn event(&mut self, id: usize, event: Event) -> Poll<Exit> {
        let player = id + 1;
        // println!("p{}: {}", player, event);
        match event {
            Event::Disconnect => {
                self.controllers.swap_remove(id);
            }
            Event::JoyY(y) => {
                *self.power.lock().unwrap() = y;
                println!("Y: {}", y);
            }
            Event::CamX(x) => {
                *self.dir.lock().unwrap() = x;
                println!("X: {}", x);
            }
            Event::MenuR(true) => return Ready(player),
            _ => {}
        }
        Pending
    }
}

const SERVER: &str = "37.59.106.4:9500";

fn handle_message(msg: Message) -> Result<()> {
    match msg {
        Message::SlaveAnnounce {
            id,
            send,
            server_receive,
            server_send,
        } => {
            // let now_ns = SystemTime::now()
            //     .duration_since(UNIX_EPOCH)
            //     .unwrap()
            //     .as_nanos();
            //
            // let elapsed_total = now_ns as i128 - send as i128;
            // let elapsed_slave = server_receive as i128 - send as i128;
            // let elapsed_master = now_ns as i128 - server_send as i128;
            //
            // println!(
            //     "One-way latency: total={:.3} ms, Slave→Server={:.3} ms, Server→Master={:.3} ms",
            //     elapsed_total as f64 / 1_000_000.0,
            //     elapsed_slave as f64 / 1_000_000.0,
            //     elapsed_master as f64 / 1_000_000.0
            // );
        }
        _ => {}
    }
    Ok(())
}

fn receive_messages(receiver: Receiver<SocketEvent>) {
    let server = SERVER.parse().unwrap();
    let bincode_config = bincode::config::standard();

    loop {
        match receiver.recv() {
            Err(e) => {
                println!("Something went wrong when receiving, error: {:?}", e);
            }
            Ok(event) => match event {
                SocketEvent::Packet(packet) => {
                    if packet.addr() == server {
                        match bincode::decode_from_slice::<Message, _>(
                            packet.payload(),
                            bincode_config,
                        ) {
                            Err(err) => eprintln!("decoding error {:?}", err),
                            Ok((msg, size)) => match handle_message(msg) {
                                Err(err) => eprintln!("handling error {:?}", err),
                                _ => {}
                            },
                        }
                    } else {
                        println!("Unknown sender.");
                    }
                }
                _ => {}
            },
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = "0.0.0.0:14000";
    let server: SocketAddr = SERVER.parse().unwrap();
    let slave = "1";

    let mut socket = Socket::bind(addr)?;
    let receiver = socket.get_event_receiver();
    let sender = socket.get_packet_sender();

    let message: Message = Message::MasterSubscribe {
        slave_id: "1".to_string(),
    };
    sender.send(Packet::reliable_unordered(
        server,
        bincode::encode_to_vec(&message, bincode::config::standard()).unwrap(),
    ))?;

    let _thread = thread::spawn(move || socket.start_polling());
    let _thread = thread::spawn(|| receive_messages(receiver));

    let state = State {
        listener: Listener::default(),
        sender: sender.clone(),
        server: server.clone(),
        controllers: Vec::new(),
        rumble: (0.0, 0.0),
        dir: Arc::new(Mutex::new(0.)),
        power: Arc::new(Mutex::new(0.)),
        power_max: 110,
    };

    let cl_dir = state.dir.clone();
    let cl_power = state.power.clone();
    let _thread = thread::spawn(move || {
        loop {
            let mut dir = (0 as i16, 0 as i16);
            {
                dir = to_motor_vector(
                    cl_dir.lock().unwrap().clone(),
                    cl_power.lock().unwrap().clone(),
                );
            }
            let mut message: Message = Message::MasterInput {
                slave_id: "1".to_string(),
                throttle: dir.1,
                steering: dir.0,
            };
            sender
                .send(Packet::unreliable(
                    server,
                    bincode::encode_to_vec(&message, bincode::config::standard()).unwrap(),
                ))
                .unwrap();
            thread::sleep(Duration::from_millis(50));
        }
    });

    gtk::init().unwrap();
    gstreamer::init().unwrap();

    let app = Application::builder()
        .application_id("com.example.videoplayer")
        .build();

    app.connect_activate(build_ui);
    app.run();
    Ok(())
}

fn choose_h264_decoder() -> &'static str {
    let registry = Registry::get();

    if registry
        .find_feature("nvh264dec", gstreamer::ElementFactory::static_type())
        .is_some()
    {
        println!("Using NVIDIA NVDEC");
        "nvh264dec"
    } else if registry
        .find_feature("vah264dec", gstreamer::ElementFactory::static_type())
        .is_some()
    {
        println!("Using VAAPI (new)");
        "vah264dec"
    } else if registry
        .find_feature("vaapih264dec", gstreamer::ElementFactory::static_type())
        .is_some()
    {
        println!("Using VAAPI (legacy)");
        "vaapih264dec"
    } else {
        println!("Using software decoder");
        "avdec_h264"
    }
}

struct VideoData {
    appsrc: gst_app::AppSrc,
    jitter: gst::Element,
    depay: gst::Element,
    parse: gst::Element,
    dec: gst::Element,
    conv: gst::Element,
    pipeline: gst::Pipeline
}

fn build_video(sink: gst::Element) -> Result<VideoData> {
    let pipeline = gst::Pipeline::new();

    let appsrc = gst::ElementFactory::make("appsrc")
        .name("source")
        .build()?;

    let queue = gst::ElementFactory::make("queue").build()?;
    queue.set_property("max-size-buffers", 2u32);
    queue.set_property_from_str("leaky", "downstream"); // downstream

    let jitter = gst::ElementFactory::make("rtpjitterbuffer")
        .build()?;

    let depay = gst::ElementFactory::make("rtph264depay")
        .build()?;

    let parse = gst::ElementFactory::make("h264parse")
        .build()?;

    let decoder = choose_h264_decoder();
    // let decoder = "avdec_h264";
    let dec = gst::ElementFactory::make(decoder)
        .build()?;

    let conv = gst::ElementFactory::make("videoconvert")
        .build()?;

    pipeline
        .add_many([&appsrc, &queue, &jitter, &depay, &parse, &dec, &conv, &sink])?;

    gst::Element::link_many([
        &appsrc,
        &queue,
        &jitter,
        &depay,
        &parse,
        &dec,
        &conv,
        &sink,
    ])?;

    // --- Configure appsrc caps for RTP ---
    let caps = gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", "H264")
        .field("payload", 96i32)
        .field("clock-rate", 90000i32)
        .build();

    let appsrc = appsrc
        .downcast::<gst_app::AppSrc>()
        .unwrap();

    appsrc.set_caps(Some(&caps));
    appsrc.set_property("is-live", true);
    appsrc.set_property("format", gst::Format::Time);
    appsrc.set_property("do-timestamp", false); // true

    // --- Ultra low latency settings ---
    jitter.set_property("latency", 0u32);
    jitter.set_property("drop-on-latency", true);

    // dec.set_property("disable-dpb", true); // FOR NVIDA

    sink.set_property("sync", false);

    Ok(VideoData{appsrc, jitter, depay, parse, dec, conv, pipeline})
}

fn build_ui(app: &Application) {
    let sink = gst::ElementFactory::make("gtk4paintablesink")
        .build().unwrap();
    sink.set_property("sync", false);

    let paintable: gtk::gdk::Paintable = sink.property("paintable");
    let picture = gtk::Picture::for_paintable(&paintable);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("CS:GO Stream Viewer")
        .default_width(1280)
        .default_height(720)
        .build();

    window.set_child(Some(&picture));
    window.present();

    gtk::prelude::NativeExt::realize(&window);
    while gtk::glib::MainContext::default().iteration(false) {}

    let video = build_video(sink).unwrap();
    video.pipeline.set_state(gst::State::Playing).unwrap();
    
    let appsrc = video.appsrc.clone();

    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();

    thread::spawn(move || {
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        socket.send_to(&[1], "37.59.106.4:13001").unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(500))).unwrap();

        let mut buffer = [0u8; 65535]; // RTP packets can be larger than 1500!

        while running_clone.load(Ordering::Relaxed) {
            if let Ok((size, _)) = socket.recv_from(&mut buffer) {
                let mut gst_buffer = gst::Buffer::with_size(size).unwrap();
                {
                    let buf = gst_buffer.get_mut().unwrap();
                    buf.copy_from_slice(0, &buffer[..size]).unwrap();
                    // --- Set timestamps ---
                    let pts = ClockTime::from_useconds(
                        glib::monotonic_time() as u64
                    );
                    buf.set_pts(pts);
                    buf.set_dts(pts);
                }
                // println!("{:?}", gst_buffer);
                if appsrc.push_buffer(gst_buffer).is_err() {
                    break;
                }
            }
        }
    });

    let pipeline_clone = video.pipeline.clone();
    thread::spawn(move || {
        let bus = pipeline_clone.bus().unwrap();
        for msg in bus.iter_timed(gst::ClockTime::NONE) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Error(err) => {
                    eprintln!("GST ERROR: {} {:?}", err.error(), err.debug());
                }
                MessageView::Warning(w) => {
                    eprintln!("GST WARN: {}", w.error());
                }
                MessageView::StateChanged(s) => {
                    eprintln!("State: {:?} -> {:?}", s.old(), s.current());
                }
                MessageView::Eos(_) => eprintln!("EOS"),
                _ => {}
            }
        }
    });


    // let overlay = Overlay::new();
    //
    // // Stats overlay box
    // let stats_box = GtkBox::new(Orientation::Vertical, 5);
    // stats_box.set_halign(gtk::Align::End);
    // stats_box.set_valign(gtk::Align::Start);
    // stats_box.set_margin_top(10);
    // stats_box.set_margin_end(10);
    //
    // let fps_label = Label::new(Some("FPS: --"));
    // let ping_label = Label::new(Some("Ping: --"));
    // let bitrate_label = Label::new(Some("Bitrate: --"));
    //
    // stats_box.append(&fps_label);
    // stats_box.append(&ping_label);
    // stats_box.append(&bitrate_label);
    //
    // // Apply CSS styling
    // let css_provider = gtk::CssProvider::new();
    // css_provider.load_from_data(
    //     "box { 
    //         background-color: rgba(0, 0, 0, 0.7); 
    //         border-radius: 10px; 
    //         padding: 15px;
    //     }
    //     label { 
    //         color: white; 
    //         font-size: 16px; 
    //         font-weight: bold;
    //         font-family: monospace;
    //     }",
    // );
    //
    // gtk::style_context_add_provider_for_display(
    //     &gdk::Display::default().unwrap(),
    //     &css_provider,
    //     gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    // );
    //
    // overlay.add_overlay(&stats_box);
    // window.set_child(Some(&overlay));
    //
    // // Update stats
    // let stats = Arc::new(Mutex::new(VideoStats {
    //     fps: 0.0,
    //     bitrate: 0,
    //     ping: 0,
    // }));
    //
    // // let sink_pad = pipeline
    // //     .by_name("video_sink")
    // //     .unwrap()
    // //     .static_pad("sink")
    // //     .unwrap();
    // //
    // // let stats_clone = stats.clone();
    // // sink_pad.add_probe(gstreamer::PadProbeType::BUFFER, move |_, _| {
    // //     stats_clone.lock().unwrap().fps += 1.;
    // //     // increment frame counter here
    // //     gstreamer::PadProbeReturn::Ok
    // // });
    //
    // let stats_clone = stats.clone();
    // let fps_label_clone = fps_label.clone();
    // let ping_label_clone = ping_label.clone();
    // let bitrate_label_clone = bitrate_label.clone();
    //
    // glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
    //     let stats = stats_clone.lock().unwrap();
    //     fps_label_clone.set_text(&format!("FPS: {:.1}", stats.fps));
    //     ping_label_clone.set_text(&format!("Ping: {} ms", stats.ping));
    //     bitrate_label_clone.set_text(&format!(
    //         "Bitrate: {:.1} Mbps",
    //         stats.bitrate as f64 / 1_000_000.0
    //     ));
    //     glib::ControlFlow::Continue
    // });


    window.connect_close_request(move |_| {
        video.pipeline.set_state(gstreamer::State::Null).unwrap();
        glib::Propagation::Proceed
    });
}
