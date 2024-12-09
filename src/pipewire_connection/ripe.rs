#![allow(clippy::single_match)]

use std::collections::HashMap;
use std::io;
use std::os::fd::AsRawFd;

use log::info;
use ripewire::reexports::libspa_consts::{SpaMediaType, SpaParamType};
use ripewire::reexports::{libspa_consts, nix, pod, pod_v2};

use calloop::{generic::Generic, EventLoop, Interest, Mode, PostAction};
use libspa_consts::SpaEnum;
use pod::dictionary::Dictionary;

use ripewire::connection::MessageBuffer;
use ripewire::context::Context;
use ripewire::protocol::{pw_core, pw_link, pw_node, pw_port, pw_registry, ParamFlags};
use ripewire::proxy::{PwCore, PwLink, PwNode, PwPort, PwRegistry};

use crate::GtkMessage;

use super::state::Item;

fn properties() -> Dictionary {
    let host = nix::unistd::gethostname().unwrap();
    let host: &str = &host.to_string_lossy();

    let uid = nix::unistd::getuid();
    let user = nix::unistd::User::from_uid(uid).unwrap().unwrap();

    let pid = nix::unistd::getpid().to_string();

    Dictionary::from([
        ("log.level", "0"),
        ("cpu.max-align", "32"),
        ("default.clock.rate", "48000"),
        ("default.clock.quantum", "1024"),
        ("default.clock.min-quantum", "32"),
        ("default.clock.max-quantum", "2048"),
        ("default.clock.quantum-limit", "8192"),
        ("default.video.width", "640"),
        ("default.video.height", "480"),
        ("default.video.rate.num", "25"),
        ("default.video.rate.denom", "1"),
        ("clock.power-of-two-quantum", "true"),
        ("link.max-buffers", "64"),
        ("mem.warn-mlock", "false"),
        ("mem.allow-mlock", "true"),
        ("settings.check-quantum", "false"),
        ("settings.check-rate", "false"),
        ("application.name", "ripewire"),
        ("application.process.binary", "ripewire"),
        ("application.language", "en_US.UTF-8"),
        ("application.process.id", &pid),
        ("application.process.user", &user.name),
        ("application.process.host", host),
        ("window.x11.display", ":0"),
        ("core.version", "0.3.58"),
        ("core.name", "pipewire-poly-185501"),
    ])
}

pub fn run(
    gtk_sender: async_channel::Sender<crate::PipewireMessage>,
    pw_receiver: calloop::channel::Channel<GtkMessage>,
) {
    let mut ctx = Context::<PipewireState>::connect("/run/user/1000/pipewire-0").unwrap();
    ripewire::set_blocking(ctx.as_raw_fd(), false);
    let core = ctx.core();
    let client = ctx.client();

    core.hello(&mut ctx);

    client.update_properties(&mut ctx, properties());

    let registry = core.get_registry(&mut ctx);

    core.sync(&mut ctx, 0, 0);

    ctx.set_object_callback(&core, PipewireState::core_event);
    ctx.set_object_callback(&registry, PipewireState::registry_event);

    let mut ev = EventLoop::<CalloopState>::try_new().unwrap();

    let fd = ctx.as_raw_fd();
    let mut state = CalloopState {
        ctx,
        state: PipewireState {
            core,
            registry,
            gtk_sender,
            binded_globals: HashMap::new(),
            state: super::State::new(),
        },
    };

    let loop_signal = ev.get_signal();
    ev.handle()
        .insert_source(pw_receiver, move |event, _, state| {
            let calloop::channel::Event::Msg(msg) = event else {
                return;
            };

            match msg {
                GtkMessage::ToggleLink { port_from, port_to } => {
                    toggle_link(
                        &mut state.ctx,
                        port_from,
                        port_to,
                        &state.state.core,
                        &state.state.registry,
                        &mut state.state.state,
                    );
                }
                GtkMessage::Connect(_) => {}
                GtkMessage::Terminate => {
                    loop_signal.stop();
                }
            }
        })
        .unwrap();

    let mut buffer = MessageBuffer::new();
    ev.handle()
        .insert_source(
            Generic::new(
                unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
                Interest::READ,
                Mode::Level,
            ),
            move |_, _, state| {
                loop {
                    let msg = state.ctx.rcv_msg(&mut buffer);

                    if let Err(err) = &msg {
                        if err.kind() == io::ErrorKind::WouldBlock {
                            break;
                        }
                    }

                    let msg = msg.unwrap();

                    state.ctx.dispatch_event(&mut state.state, msg);
                }

                Ok(PostAction::Continue)
            },
        )
        .unwrap();

    ev.run(None, &mut state, |_state| {}).unwrap();
}

struct CalloopState {
    ctx: Context<PipewireState>,
    state: PipewireState,
}

enum BindedGlobal {
    Node(PwNode),
    Port { port: PwPort, node_id: Option<u32> },
    Link(PwLink),
}

struct PipewireState {
    core: PwCore,
    registry: PwRegistry,
    gtk_sender: async_channel::Sender<crate::PipewireMessage>,

    binded_globals: HashMap<u32, BindedGlobal>,

    state: super::State,
}

impl PipewireState {
    pub fn core_event(
        &mut self,
        ctx: &mut Context<Self>,
        core: PwCore,
        core_event: pw_core::Event,
    ) {
        match core_event {
            pw_core::Event::Ping(ping) => {
                core.pong(ctx, ping.id, ping.seq);
            }
            _ => {}
        }
    }

    pub fn node_event(
        &mut self,
        _ctx: &mut Context<Self>,
        _node: PwNode,
        node_event: pw_node::Event,
    ) {
        match node_event {
            pw_node::Event::Info(info) => {
                if let Some(media_name) = info.props.get("media.name") {
                    let name = info
                        .props
                        .get("node.description")
                        .or_else(|| info.props.get("node.nick"))
                        .or_else(|| info.props.get("node.name"))
                        .cloned()
                        .unwrap_or_default();

                    self.gtk_sender
                        .send_blocking(crate::PipewireMessage::NodeNameChanged {
                            id: info.id,
                            name,
                            media_name: media_name.to_string(),
                        })
                        .expect("Failed to send message");
                }
            }
            pw_node::Event::Param(_) => {}
        }
    }

    pub fn registry_event(
        &mut self,
        ctx: &mut Context<Self>,
        _registry: PwRegistry,
        registry_event: pw_registry::Event,
    ) {
        match registry_event {
            pw_registry::Event::Global(global) => match global.interface {
                ripewire::object_map::ObjectType::Node => {
                    let name = global
                        .properties
                        .get("node.description")
                        .or_else(|| global.properties.get("node.nick"))
                        .or_else(|| global.properties.get("node.name"))
                        .cloned()
                        .unwrap_or_default();

                    let media_class = |class: &String| {
                        if class.contains("Sink") || class.contains("Input") {
                            Some(crate::NodeType::Input)
                        } else if class.contains("Source") || class.contains("Output") {
                            Some(crate::NodeType::Output)
                        } else {
                            None
                        }
                    };

                    let node_type = global
                        .properties
                        .get("media.category")
                        .and_then(|class| {
                            if class.contains("Duplex") {
                                None
                            } else {
                                global.properties.get("media.class").and_then(media_class)
                            }
                        })
                        .or_else(|| global.properties.get("media.class").and_then(media_class));

                    self.state.insert(global.id, Item::Node);
                    self.gtk_sender
                        .send_blocking(crate::PipewireMessage::NodeAdded {
                            id: global.id,
                            name,
                            node_type,
                        })
                        .expect("Failed to send message");

                    let node: PwNode = self.registry.bind(ctx, &global);

                    self.binded_globals
                        .insert(global.id, BindedGlobal::Node(node.clone()));

                    ctx.set_object_callback(&node, Self::node_event);
                }
                ripewire::object_map::ObjectType::Link => {
                    let link: PwLink = self.registry.bind(ctx, &global);

                    self.binded_globals
                        .insert(global.id, BindedGlobal::Link(link.clone()));

                    ctx.set_object_callback_with_data(&link, true, Self::link_event);
                }
                ripewire::object_map::ObjectType::Port => {
                    let port: PwPort = self.registry.bind(ctx, &global);

                    self.binded_globals.insert(
                        global.id,
                        BindedGlobal::Port {
                            port: port.clone(),
                            node_id: None,
                        },
                    );

                    ctx.set_object_callback_with_data(&port, (global.id, true), Self::port_event);
                }
                _ => {}
            },
            pw_registry::Event::GlobalRemove(msg) => {
                if let Some(obj) = self.binded_globals.remove(&msg.id) {
                    self.core.destroy_object(
                        ctx,
                        match &obj {
                            BindedGlobal::Node(obj) => obj.id(),
                            BindedGlobal::Port { port, .. } => port.id(),
                            BindedGlobal::Link(obj) => obj.id(),
                        },
                    );

                    let gtk_msg = match obj {
                        BindedGlobal::Node(_) => crate::PipewireMessage::NodeRemoved { id: msg.id },
                        BindedGlobal::Port { node_id, .. } => crate::PipewireMessage::PortRemoved {
                            id: msg.id,
                            node_id: node_id.unwrap(),
                        },
                        BindedGlobal::Link(_) => crate::PipewireMessage::LinkRemoved { id: msg.id },
                    };

                    self.state.remove(msg.id);
                    self.gtk_sender
                        .send_blocking(gtk_msg)
                        .expect("Failed to send message");
                }
            }
        }
    }

    pub fn link_event(
        &mut self,
        _ctx: &mut Context<Self>,
        is_first_info_event: &mut bool,
        _link: PwLink,
        link_event: pw_link::Event,
    ) {
        match link_event {
            pw_link::Event::Info(info) => {
                if *is_first_info_event {
                    let port_from = info.output_port_id;
                    let port_to = info.input_port_id;
                    let format = info
                        .format
                        .as_deserializer()
                        .as_object()
                        .map(pod_v2::obj_gen::untyped::Format);

                    self.state
                        .insert(info.id, Item::Link { port_from, port_to });
                    self.gtk_sender
                        .send_blocking(crate::PipewireMessage::LinkAdded {
                            id: info.id,
                            port_from,
                            port_to,
                            active: matches!(
                                info.state,
                                SpaEnum::Value(libspa_consts::PwLinkState::Active)
                            ),
                            media_type: format
                                .ok()
                                .and_then(|f| f.media_type())
                                .and_then(|pod| pod.as_id().ok())
                                .map(libspa_consts::SpaEnum::from_raw)
                                .map(|v: libspa_consts::SpaEnum<libspa_consts::SpaMediaType>| {
                                    v.unwrap()
                                })
                                .unwrap_or(SpaMediaType::Unknown),
                        })
                        .expect("Failed to send message");
                } else {
                    // Info was an update - figure out if we should notify the gtk thread
                    if info
                        .change_mask
                        .contains(ripewire::protocol::pw_link::ChangeMask::STATE)
                    {
                        self.gtk_sender
                            .send_blocking(crate::PipewireMessage::LinkStateChanged {
                                id: info.id,
                                active: matches!(
                                    info.state,
                                    SpaEnum::Value(libspa_consts::PwLinkState::Active)
                                ),
                            })
                            .expect("Failed to send message");
                    }
                    if info
                        .change_mask
                        .contains(ripewire::protocol::pw_link::ChangeMask::FORMAT)
                    {
                        let format = info
                            .format
                            .as_deserializer()
                            .as_object()
                            .map(pod_v2::obj_gen::untyped::Format);

                        self.gtk_sender
                            .send_blocking(crate::PipewireMessage::LinkFormatChanged {
                                id: info.id,
                                media_type: format
                                    .ok()
                                    .and_then(|f| f.media_type())
                                    .and_then(|pod| pod.as_id().ok())
                                    .map(libspa_consts::SpaEnum::<libspa_consts::SpaMediaType>::from_raw)
                                    .map(libspa_consts::SpaEnum::unwrap)
                                    .unwrap_or(SpaMediaType::Unknown),
                            })
                            .expect("Failed to send message");
                    }
                }

                *is_first_info_event = false;
            }
        }
    }

    pub fn port_event(
        &mut self,
        ctx: &mut Context<Self>,
        (port_id, is_first_info_event): &mut (u32, bool),
        proxy: PwPort,
        port_event: pw_port::Event,
    ) {
        match port_event {
            pw_port::Event::Info(info) => {
                if *is_first_info_event {
                    let name = info.props.get("port.name").cloned().unwrap_or_default();
                    let node_id: u32 = info
                        .props
                        .get("node.id")
                        .expect("Port has no node.id property!")
                        .parse()
                        .expect("Could not parse node.id property");

                    if let BindedGlobal::Port { node_id: id, .. } =
                        self.binded_globals.get_mut(port_id).unwrap()
                    {
                        *id = Some(node_id);
                    }

                    self.state.insert(info.id, Item::Port { node_id });

                    let enum_format_info = info
                        .params
                        .iter()
                        .find(|param| param.id == SpaEnum::Value(SpaParamType::EnumFormat));
                    if let Some(enum_format_info) = enum_format_info {
                        if enum_format_info.flags.contains(ParamFlags::READ) {
                            proxy.enum_params(ctx, SpaParamType::EnumFormat);
                        }
                    }

                    self.gtk_sender
                        .send_blocking(crate::PipewireMessage::PortAdded {
                            id: info.id,
                            node_id,
                            name,
                            direction: info.direction.unwrap(),
                        })
                        .expect("Failed to send message");
                }

                *is_first_info_event = false;
            }
            pw_port::Event::Param(param) => {
                if param.id != SpaEnum::Value(SpaParamType::EnumFormat) {
                    return;
                }

                let format = pod_v2::obj_gen::untyped::Format(
                    param.params.as_deserializer().as_object().unwrap(),
                );

                let media_type = format
                    .media_type()
                    .and_then(|pod| pod.as_id().ok())
                    .map(libspa_consts::SpaEnum::from_raw)
                    .map(libspa_consts::SpaEnum::unwrap)
                    .unwrap_or(SpaMediaType::Unknown);

                self.gtk_sender
                    .send_blocking(crate::PipewireMessage::PortFormatChanged {
                        id: *port_id,
                        media_type,
                    })
                    .expect("Failed to send message")
            }
        }
    }
}

/// Toggle a link between the two specified ports.
fn toggle_link<D>(
    ctx: &mut Context<D>,
    port_from: u32,
    port_to: u32,
    core: &PwCore,
    registry: &PwRegistry,
    state: &mut super::State,
) {
    if let Some(id) = state.get_link_id(port_from, port_to) {
        info!("Requesting removal of link with id {}", id);

        // FIXME: Handle error
        registry.destroy_global(ctx, id);
    } else {
        info!(
            "Requesting creation of link from port id:{} to port id:{}",
            port_from, port_to
        );

        let node_from = state
            .get_node_of_port(port_from)
            .expect("Requested port not in state");
        let node_to = state
            .get_node_of_port(port_to)
            .expect("Requested port not in state");

        let _link: PwLink = core.create_object(
            ctx,
            pw_core::methods::CreateObject {
                factory_name: "link-factory".into(),
                interface: "PipeWire:Interface:Link".into(),
                version: 3,
                properties: Dictionary::from([
                    ("link.output.node", node_from.to_string().as_str()),
                    ("link.output.port", port_from.to_string().as_str()),
                    ("link.input.node", node_to.to_string().as_str()),
                    ("link.input.port", port_to.to_string().as_str()),
                    ("object.linger", "1"),
                ]),
                new_id: 0,
            },
        );
    }
}
