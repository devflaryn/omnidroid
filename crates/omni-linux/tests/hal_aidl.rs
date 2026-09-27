//! The generated AIDL code (`tools/gen_aidl.py` -> `hal/aidl/`) at the parcel level: composer3 types
//! round-trip through `Writer`/`Reader`, their bytes are what the NDK backend puts on the wire, and a
//! generated dispatcher answers requests built by hand from the wire format (independently of the
//! generated writers), refusing malformed ones with `EX_ILLEGAL_ARGUMENT` rather than panicking.
use std::sync::{Arc, Mutex};

use omni_linux::binder::{broker, Context, HostCall, HostReply};
use omni_linux::fd::{FileKind, OpenFile};
use omni_linux::hal::aidl::android_hardware_common::NativeHandle;
use omni_linux::hal::aidl::android_hardware_graphics_common as common;
use omni_linux::hal::aidl::android_hardware_graphics_composer3 as c3;
use omni_linux::hal::aidl::{Binder, CallError, Ctx, Fd, Status};
use omni_linux::hal::parcel::{Malformed, Reader, Writer};

const CLIENT: &str = "android.hardware.graphics.composer3.IComposerClient";
const CALLBACK: &str = "android.hardware.graphics.composer3.IComposerCallback";
const COMPOSER_HASH: &str = "d24fcd9648b8b2e7287f9238eee9180244612c10";
const TYPE_FD: u32 = 0x6664_2a85;
const TYPE_BINDER: u32 = 0x7362_2a85;
const TYPE_HANDLE: u32 = 0x7368_2a85;
const STABILITY_VINTF: i32 = 0b11_1111;

fn file(name: &str) -> Arc<OpenFile> {
    let kind = FileKind::Synth { data: name.as_bytes().to_vec(), guest: name.as_bytes().to_vec(), pos: 0, sized: true };
    Arc::new(OpenFile { kind: parking_lot::Mutex::new(kind), flags: parking_lot::Mutex::new(0) })
}

/// What a `Writer` wrote, as a transaction carries it: the object offsets in order, and the files
/// of its `TYPE_FD` objects in object order.
fn objects(w: &Writer) -> (Vec<u64>, Vec<Arc<OpenFile>>) {
    let mut objs: Vec<(usize, Option<Arc<OpenFile>>)> =
        w.fds.iter().map(|(o, f)| (*o, Some(Arc::clone(f)))).chain(w.binders.iter().map(|&o| (o, None))).collect();
    objs.sort_by_key(|o| o.0);
    let offsets = objs.iter().map(|o| o.0 as u64).collect();
    let fds = objs.into_iter().filter_map(|o| o.1).collect();
    (offsets, fds)
}

fn words(b: &[u8]) -> Vec<i32> {
    b.chunks_exact(4).map(|c| i32::from_le_bytes(c.try_into().unwrap())).collect()
}

/// A parcel built by hand from the wire format.
#[derive(Default)]
struct Raw(Vec<u8>);

impl Raw {
    fn i32(mut self, v: i32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn i64(mut self, v: i64) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn s16(mut self, s: &str) -> Self {
        let units: Vec<u16> = s.encode_utf16().collect();
        self = self.i32(units.len() as i32);
        for u in units.iter().chain([0u16].iter()) {
            self.0.extend_from_slice(&u.to_le_bytes());
        }
        while self.0.len() & 3 != 0 {
            self.0.push(0);
        }
        self
    }

    /// The interface token as a guest's libbinder writes it.
    fn token(self, descriptor: &str) -> Self {
        self.i32(i32::MIN).i32(-1).i32(0x5359_5354).s16(descriptor)
    }

    /// A `flat_binder_object`.
    fn object(mut self, kind: u32, value: u64) -> Self {
        self.0.extend_from_slice(&kind.to_le_bytes());
        self.0.extend_from_slice(&0u32.to_le_bytes());
        self.0.extend_from_slice(&value.to_le_bytes());
        self.0.extend_from_slice(&0u64.to_le_bytes());
        self
    }

    fn at(&self) -> u64 {
        self.0.len() as u64
    }
}

fn call(code: u32, data: Vec<u8>, offsets: Vec<u64>, fds: Vec<Arc<OpenFile>>) -> HostCall {
    HostCall { code, data, offsets, fds, handles: vec![], sender_pid: 4242, sender_euid: 1000 }
}

// ---------------------------------------------------------------------------------------------
// Types

fn sample_command(buffer_fd: &Arc<OpenFile>, fence: &Arc<OpenFile>) -> c3::DisplayCommand {
    let layer = c3::LayerCommand {
        layer: 3,
        cursor_position: Some(common::Point { x: 5, y: -6 }),
        buffer: Some(c3::Buffer {
            slot: 1,
            handle: Some(NativeHandle { fds: vec![Fd(Arc::clone(buffer_fd))], ints: vec![0x4247_4d4f, 1, 64, 32] }),
            fence: Some(Fd(Arc::clone(fence))),
        }),
        damage: Some(vec![Some(common::Rect { left: 0, top: 0, right: 64, bottom: 32 }), None]),
        blend_mode: Some(c3::ParcelableBlendMode { blend_mode: common::BlendMode::PREMULTIPLIED }),
        composition: Some(c3::ParcelableComposition { composition: c3::Composition::DEVICE }),
        display_frame: Some(common::Rect { left: 1, top: 2, right: 3, bottom: 4 }),
        source_crop: Some(common::FRect { left: 0.0, top: 0.5, right: 64.0, bottom: 32.25 }),
        z: Some(c3::ZOrder { z: 2 }),
        color_transform: Some(vec![1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]),
        per_frame_metadata_blob: Some(vec![Some(c3::PerFrameMetadataBlob { key: c3::PerFrameMetadataKey::HDR10_PLUS_SEI, blob: vec![1, 2, 3, 4, 5] })]),
        buffer_slots_to_clear: Some(vec![4, 5]),
        layer_lifecycle_batch_command_type: c3::LayerLifecycleBatchCommandType::CREATE,
        new_buffer_slot_count: 3,
        ..Default::default()
    };
    c3::DisplayCommand {
        display: 7,
        layers: vec![layer],
        brightness: Some(c3::DisplayBrightness { brightness: 0.5, brightness_nits: 200.0 }),
        client_target: Some(c3::ClientTarget { dataspace: common::Dataspace::SRGB, damage: vec![], ..Default::default() }),
        expected_present_time: Some(c3::ClockMonotonicTimestamp { timestamp_nanos: 123_456_789_000 }),
        validate_display: true,
        present_display: true,
        frame_interval_ns: 16_666_666,
        ..Default::default()
    }
}

#[test]
fn a_display_command_round_trips_with_its_files() {
    let (buffer_fd, fence) = (file("buffer"), file("fence"));
    let cmd = sample_command(&buffer_fd, &fence);
    let mut w = Writer::new();
    cmd.write_value(&mut w);
    // The NDK's parcelable header: non-null, then the size of the whole parcelable.
    let head = words(&w.data[..16]);
    assert_eq!(head[0], 1);
    assert_eq!(head[1] as usize, w.data.len() - 4);
    assert_eq!(i64::from_le_bytes(w.data[8..16].try_into().unwrap()), 7, "display, unaligned i64 right after the size");
    // The two files, in the order they were written: the handle's, then the fence.
    assert_eq!(w.fds.len(), 2);
    assert!(Arc::ptr_eq(&w.fds[0].1, &buffer_fd) && Arc::ptr_eq(&w.fds[1].1, &fence));
    for (at, _) in &w.fds {
        assert_eq!(u32::from_le_bytes(w.data[*at..*at + 4].try_into().unwrap()), TYPE_FD);
        // A ParcelFileDescriptor: non-null, no comm channel, then the object.
        assert_eq!(words(&w.data[*at - 8..*at]), [1, 0]);
    }

    let (offsets, fds) = objects(&w);
    let mut r = Reader::with_objects(&w.data, &offsets, &fds);
    let back = c3::DisplayCommand::read_value(&mut r).unwrap();
    assert_eq!(r.position(), w.data.len());
    assert_eq!(back, cmd);
    let layer = &back.layers[0];
    let buffer = layer.buffer.as_ref().unwrap();
    assert!(Arc::ptr_eq(&buffer.handle.as_ref().unwrap().fds[0].0, &buffer_fd));
    assert!(Arc::ptr_eq(&buffer.fence.as_ref().unwrap().0, &fence));
    assert_eq!(back.client_target.as_ref().unwrap().hdr_sdr_ratio, 1.0);
}

#[test]
fn a_file_the_transaction_did_not_list_is_malformed() {
    let (buffer_fd, fence) = (file("buffer"), file("fence"));
    let mut w = Writer::new();
    sample_command(&buffer_fd, &fence).write_value(&mut w);
    let (offsets, fds) = objects(&w);
    // No objects listed: the fd objects are bytes a guest wrote, not descriptors it sent.
    assert!(c3::DisplayCommand::read_value(&mut Reader::with_objects(&w.data, &[], &[])).is_err());
    // Listed, but the transaction carried fewer files than it listed.
    assert!(c3::DisplayCommand::read_value(&mut Reader::with_objects(&w.data, &offsets, &fds[..1])).is_err());
}

#[test]
fn defaults_are_the_aidl_defaults() {
    assert_eq!(c3::ClientTarget::default().hdr_sdr_ratio, 1.0);
    let d = common::HardwareBufferDescription::default();
    assert_eq!(d.format, common::PixelFormat::UNSPECIFIED);
    assert_eq!(d.usage, common::BufferUsage::CPU_READ_NEVER);
    assert_eq!(common::HdrConversionStrategy::default(), common::HdrConversionStrategy::Passthrough(true));
    assert_eq!(c3::LayerCommand::default().buffer, None);
    assert_eq!(common::BufferUsage::VENDOR_MASK_HI.0, -281_474_976_710_656);
    assert_eq!(common::Transform::ROT_270.0, 7);
    assert_eq!(common::Dataspace::SRGB.0, 142_671_872);
    assert_eq!(common::DisplayHotplugEvent::ERROR_TOO_MANY_DISPLAYS.0, -3);
    assert_eq!(c3::PerFrameMetadataKey::HDR10_PLUS_SEI.0, 12);
    assert_eq!(c3::PresentOrValidate_Result::Presented.0, 1i8);
    assert_eq!(c3::FormatColorComponent::FORMAT_COMPONENT_3.0, 8i8);
    assert_eq!(c3::DisplayRequest::WRITE_CLIENT_TARGET_TO_OUTPUT, 2);
    assert_eq!(c3::DisplayRequest_LayerRequest::CLEAR_CLIENT_TARGET, 1);
    assert_eq!(c3::i_composer_client::INVALID_CONFIGURATION, 0x7fff_ffff);
    assert_eq!(c3::i_composer::EX_NO_RESOURCES, 6);
}

#[test]
fn an_older_writers_missing_fields_keep_defaults_and_a_newer_writers_extra_fields_are_skipped() {
    // A ClientTarget from a writer that knew only `buffer` and `dataspace`.
    let old = Raw::default()
        .i32(1)
        .i32(4 + 4 + 16 + 4) // size: itself, buffer's marker, buffer (size, slot, null handle, null fence), dataspace
        .i32(1)
        .i32(16)
        .i32(9)
        .i32(0)
        .i32(0)
        .i32(common::Dataspace::SRGB.0)
        .i32(0x7777); // what follows the parcelable
    let mut r = Reader::new(&old.0);
    let t = c3::ClientTarget::read_value(&mut r).unwrap();
    assert_eq!(t.buffer, c3::Buffer { slot: 9, handle: None, fence: None });
    assert_eq!(t.dataspace, common::Dataspace::SRGB);
    assert!(t.damage.is_empty());
    assert_eq!(t.hdr_sdr_ratio, 1.0);
    assert_eq!(r.i32().unwrap(), 0x7777);

    // A Rect from a writer with a fifth field.
    let new = Raw::default().i32(1).i32(24).i32(1).i32(2).i32(3).i32(4).i32(5).i32(0x7777);
    let mut r = Reader::new(&new.0);
    assert_eq!(common::Rect::read_value(&mut r).unwrap(), common::Rect { left: 1, top: 2, right: 3, bottom: 4 });
    assert_eq!(r.i32().unwrap(), 0x7777);

    // A PresentFence missing its (required) fence cannot default it.
    let short = Raw::default().i32(1).i32(12).i64(5);
    assert_eq!(c3::PresentFence::read_value(&mut Reader::new(&short.0)), Err(Malformed("PresentFence.fence is missing")));

    // Sizes that lie.
    for size in [0, 3, -4, 1000] {
        let bad = Raw::default().i32(1).i32(size).i32(1).i32(2).i32(3).i32(4);
        assert!(common::Rect::read_value(&mut Reader::new(&bad.0)).is_err(), "size {size}");
    }
    // A null where a value is required.
    assert!(common::Rect::read_value(&mut Reader::new(&Raw::default().i32(0).0)).is_err());
    assert_eq!(common::Rect::read_nullable(&mut Reader::new(&Raw::default().i32(0).0)), Ok(None));
}

#[test]
fn command_result_payload_variants_are_a_tag_and_the_field() {
    let fence = file("present");
    let release = file("release");
    let payloads = [
        c3::CommandResultPayload::Error(c3::CommandError { command_index: 2, error_code: 8 }),
        c3::CommandResultPayload::ChangedCompositionTypes(c3::ChangedCompositionTypes {
            display: 1,
            layers: vec![c3::ChangedCompositionLayer { layer: 9, composition: c3::Composition::CLIENT }],
        }),
        c3::CommandResultPayload::DisplayRequest(c3::DisplayRequest {
            display: 1,
            mask: c3::DisplayRequest::FLIP_CLIENT_TARGET,
            layer_requests: vec![c3::DisplayRequest_LayerRequest { layer: 9, mask: 1 }],
        }),
        c3::CommandResultPayload::PresentFence(c3::PresentFence { display: 1, fence: Fd(Arc::clone(&fence)) }),
        c3::CommandResultPayload::ReleaseFences(c3::ReleaseFences {
            display: 1,
            layers: vec![c3::ReleaseFences_Layer { layer: 9, fence: Fd(Arc::clone(&release)) }],
        }),
        c3::CommandResultPayload::PresentOrValidateResult(c3::PresentOrValidate { display: 1, result: c3::PresentOrValidate_Result::Presented }),
        c3::CommandResultPayload::ClientTargetProperty(c3::ClientTargetPropertyWithBrightness {
            display: 1,
            client_target_property: c3::ClientTargetProperty { pixel_format: common::PixelFormat::RGBA_8888, dataspace: common::Dataspace::SRGB },
            brightness: 0.75,
            dimming_stage: c3::DimmingStage::GAMMA_OETF,
        }),
    ];
    for (tag, p) in payloads.iter().enumerate() {
        assert_eq!(p.tag(), tag as i32);
        let mut w = Writer::new();
        p.write_value(&mut w);
        assert_eq!(words(&w.data[..8]), [1, tag as i32], "marker, then tag");
        let (offsets, fds) = objects(&w);
        let mut r = Reader::with_objects(&w.data, &offsets, &fds);
        assert_eq!(&c3::CommandResultPayload::read_value(&mut r).unwrap(), p);
        assert_eq!(r.position(), w.data.len());
    }
    // The error variant, byte for byte: marker, tag 0, the parcelable (marker, size 12, fields).
    let mut w = Writer::new();
    payloads[0].write_value(&mut w);
    assert_eq!(words(&w.data), [1, 0, 1, 12, 2, 8]);
    // presentOrValidateResult: `Result` has no @Backing, so it is byte-backed -- one i32.
    let mut w = Writer::new();
    payloads[5].write_value(&mut w);
    assert_eq!(words(&w.data), [1, 5, 1, 16, 1, 0, 1]);
    // A tag past the last field.
    assert!(c3::CommandResultPayload::read_value(&mut Reader::new(&Raw::default().i32(1).i32(7).i32(0).0)).is_err());
}

#[test]
fn arrays_are_a_length_then_the_elements() {
    // byte and byte[]: the byte sign-extended in an i32; the array packed and padded to 4.
    let id = c3::DisplayIdentification { port: -1, data: vec![1, 2, 3, 4, 5] };
    let mut w = Writer::new();
    id.write_value(&mut w);
    assert_eq!(w.data, Raw::default().i32(1).i32(20).i32(-1).i32(5).i32(0x0403_0201).i32(0x05).0);
    assert_eq!(c3::DisplayIdentification::read_value(&mut Reader::new(&w.data)).unwrap(), id);

    // long[]: 8 bytes each, not aligned to 8.
    let sample = c3::DisplayContentSample { frame_count: 2, sample_component0: vec![-1, 1 << 40], ..Default::default() };
    let mut w = Writer::new();
    sample.write_value(&mut w);
    let expected = Raw::default().i32(1).i32(4 + 8 + 4 + 16 + 12).i64(2).i32(2).i64(-1).i64(1 << 40).i32(0).i32(0).i32(0);
    assert_eq!(w.data, expected.0);
    assert_eq!(c3::DisplayContentSample::read_value(&mut Reader::new(&w.data)).unwrap(), sample);

    // Arrays of int-backed enums, and of parcelables (each element marked non-null).
    let overlay = c3::OverlayProperties {
        combinations: vec![c3::OverlayProperties_SupportedBufferCombinations {
            pixel_formats: vec![common::PixelFormat::RGBA_8888, common::PixelFormat::RGBA_FP16],
            standards: vec![common::Dataspace::STANDARD_BT709],
            transfers: vec![],
            ranges: vec![common::Dataspace::RANGE_FULL],
        }],
        support_mixed_color_spaces: true,
    };
    let mut w = Writer::new();
    overlay.write_value(&mut w);
    let inner = [1, 4 + 12 + 8 + 4 + 8, 2, 1, 0x16, 1, 1 << 16, 0, 1, 1 << 27];
    let mut want = vec![1, 0, 1];
    want.extend_from_slice(&inner);
    want.push(1);
    want[1] = (4 * want.len() - 4) as i32;
    assert_eq!(words(&w.data), want);
    assert_eq!(c3::OverlayProperties::read_value(&mut Reader::new(&w.data)).unwrap(), overlay);

    // A length the parcel cannot hold is refused before anything is allocated for it.
    let huge = Raw::default().i32(1).i32(16).i64(0).i32(i32::MAX);
    assert!(c3::ChangedCompositionTypes::read_value(&mut Reader::new(&huge.0)).is_err());
    // A null where the array is required.
    let null = Raw::default().i32(1).i32(16).i64(0).i32(-1);
    assert!(c3::ChangedCompositionTypes::read_value(&mut Reader::new(&null.0)).is_err());
}

// ---------------------------------------------------------------------------------------------
// The dispatcher

#[derive(Default)]
struct FakeClient {
    callbacks: Mutex<Vec<Binder>>,
    readback: Mutex<Vec<(NativeHandle, Option<Fd>)>>,
    senders: Mutex<Vec<i32>>,
}

impl c3::IComposerClientServer for FakeClient {
    fn create_layer(&self, ctx: &Ctx<'_>, display: i64, buffer_slot_count: i32) -> Result<i64, Status> {
        self.senders.lock().unwrap().push(ctx.call.sender_pid);
        if display != 0 {
            return Err(Status::ServiceSpecific(c3::i_composer_client::EX_BAD_DISPLAY));
        }
        Ok(0x1_0000_0000 + i64::from(buffer_slot_count))
    }

    fn get_display_attribute(&self, _: &Ctx<'_>, display: i64, config: i32, attribute: c3::DisplayAttribute) -> Result<i32, Status> {
        if display != 0 {
            return Err(Status::ServiceSpecific(c3::i_composer_client::EX_BAD_DISPLAY));
        }
        if config != 0 {
            return Err(Status::ServiceSpecific(c3::i_composer_client::EX_BAD_CONFIG));
        }
        match attribute {
            c3::DisplayAttribute::WIDTH => Ok(1080),
            c3::DisplayAttribute::HEIGHT => Ok(2400),
            c3::DisplayAttribute::VSYNC_PERIOD => Ok(16_666_666),
            _ => Err(Status::Exception(-3, "no such attribute".into())),
        }
    }

    fn register_callback(&self, _: &Ctx<'_>, callback: Binder) -> Result<(), Status> {
        self.callbacks.lock().unwrap().push(callback);
        Ok(())
    }

    fn set_readback_buffer(&self, _: &Ctx<'_>, _: i64, buffer: NativeHandle, release_fence: Option<Fd>) -> Result<(), Status> {
        self.readback.lock().unwrap().push((buffer, release_fence));
        Ok(())
    }

    fn execute_commands(&self, _: &Ctx<'_>, commands: Vec<c3::DisplayCommand>) -> Result<Vec<c3::CommandResultPayload>, Status> {
        Ok(commands
            .iter()
            .map(|c| c3::CommandResultPayload::PresentOrValidateResult(c3::PresentOrValidate { display: c.display, result: c3::PresentOrValidate_Result::Validated }))
            .collect())
    }
}

fn client_call(svc: &FakeClient, code: u32, data: Raw) -> HostReply {
    c3::i_composer_client::dispatch(svc, call(code, data.0, vec![], vec![]))
}

/// A reply's exception header: code, message, stack trace header, and a service-specific code.
fn exception(reply: &HostReply) -> (i32, String, Option<i32>) {
    let mut r = Reader::new(&reply.data);
    let code = r.i32().unwrap();
    let message = r.string16().unwrap().unwrap();
    assert_eq!(r.i32().unwrap(), 0, "an empty remote stack trace header");
    (code, message, (code == -8).then(|| r.i32().unwrap()))
}

#[test]
fn the_dispatcher_answers_requests_as_a_guests_libbinder_writes_them() {
    let svc = FakeClient::default();
    // getDisplayAttribute is the 9th method: code 9.
    assert_eq!(c3::i_composer_client::TRANSACTION_GET_DISPLAY_ATTRIBUTE, 9);
    assert_eq!(c3::i_composer_client::TRANSACTION_CREATE_LAYER, 1);
    let attr = |display: i64, config: i32, attribute: i32| client_call(&svc, 9, Raw::default().token(CLIENT).i64(display).i32(config).i32(attribute));

    let r = attr(0, 0, 1);
    assert_eq!(words(&r.data), [0, 1080]);
    assert_eq!(words(&attr(0, 0, 3).data), [0, 16_666_666]);
    // A service-specific error: -8, an empty message, no stack trace, the code.
    let r = attr(5, 0, 1);
    assert_eq!(words(&r.data), [-8, 0, 0, 0, 2]);
    assert_eq!(exception(&attr(0, 1, 1)), (-8, String::new(), Some(c3::i_composer_client::EX_BAD_CONFIG)));
    assert_eq!(exception(&attr(0, 0, 99)), (-3, "no such attribute".into(), None));

    // createLayer: the long comes back unaligned right after the status.
    let r = client_call(&svc, 1, Raw::default().token(CLIENT).i64(0).i32(3));
    assert_eq!(r.data, Raw::default().i32(0).i64(0x1_0000_0003).0);
    assert_eq!(*svc.senders.lock().unwrap(), [4242], "the method saw the transaction's sender");

    // A method the server did not implement.
    let r = client_call(&svc, 6, Raw::default().token(CLIENT).i64(0));
    assert_eq!(exception(&r), (-7, "IComposerClient.getActiveConfig is not implemented".into(), None));

    // The meta-transactions.
    let r = client_call(&svc, 0x00ff_ffff, Raw::default().token(CLIENT));
    assert_eq!(words(&r.data), [0, 3]);
    let r = client_call(&svc, 0x00ff_fffe, Raw::default().token(CLIENT));
    assert_eq!(r.data, Raw::default().i32(0).s16(COMPOSER_HASH).0);
    let r = client_call(&svc, 0x5f4e_5446, Raw::default());
    assert_eq!(r.data, Raw::default().s16(CLIENT).0, "INTERFACE_TRANSACTION: the descriptor, no status");
    assert!(client_call(&svc, 0x5f50_4e47, Raw::default()).data.is_empty(), "PING_TRANSACTION: an empty reply");

    // An unknown code (after a valid token, and outside the user range).
    assert_eq!(exception(&client_call(&svc, 1000, Raw::default().token(CLIENT))).0, -7);
    assert_eq!(exception(&client_call(&svc, 0x5f00_0000, Raw::default())).0, -7);
}

#[test]
fn the_dispatcher_decodes_objects_by_the_transactions_offsets() {
    let svc = FakeClient::default();
    // registerCallback (code 27) with a binder the broker translated to a host handle.
    assert_eq!(c3::i_composer_client::TRANSACTION_REGISTER_CALLBACK, 27);
    let req = Raw::default().token(CLIENT);
    let at = req.at();
    let req = req.object(TYPE_HANDLE, 7).i32(STABILITY_VINTF);
    let r = c3::i_composer_client::dispatch(&svc, call(27, req.0.clone(), vec![at], vec![]));
    assert_eq!(words(&r.data), [0]);
    assert_eq!(*svc.callbacks.lock().unwrap(), [Binder::Handle(7)]);
    // The same bytes, not listed as an object: a handle the guest made up.
    let r = c3::i_composer_client::dispatch(&svc, call(27, req.0, vec![], vec![]));
    assert_eq!(exception(&r).0, -3);
    // A null callback where one is required.
    let r = client_call(&svc, 27, Raw::default().token(CLIENT).object(TYPE_BINDER, 0).i32(0));
    assert_eq!(exception(&r).0, -3);

    // setReadbackBuffer (code 39): a NativeHandle carrying one descriptor, then a null fence.
    assert_eq!(c3::i_composer_client::TRANSACTION_SET_READBACK_BUFFER, 39);
    let buffer = file("readback");
    let req = Raw::default().token(CLIENT).i64(0).i32(1).i32(0).i32(1).i32(1).i32(0);
    let (size_at, fd_at) = (req.at() as usize - 16, req.at());
    let mut req = req.object(TYPE_FD, 12).i32(2).i32(10).i32(20).i32(0);
    let size = (req.0.len() - 4 - size_at) as i32;
    req.0[size_at..size_at + 4].copy_from_slice(&size.to_le_bytes());
    let r = c3::i_composer_client::dispatch(&svc, call(39, req.0, vec![fd_at], vec![Arc::clone(&buffer)]));
    assert_eq!(words(&r.data), [0]);
    let seen = svc.readback.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(Arc::ptr_eq(&seen[0].0.fds[0].0, &buffer));
    assert_eq!(seen[0].0.ints, [10, 20]);
    assert_eq!(seen[0].1, None);
}

#[test]
fn the_dispatcher_answers_execute_commands() {
    let svc = FakeClient::default();
    let (buffer_fd, fence) = (file("buffer"), file("fence"));
    let mut w = Writer::new();
    w.interface_token(CLIENT);
    w.i32(2);
    sample_command(&buffer_fd, &fence).write_value(&mut w);
    c3::DisplayCommand { display: 9, ..Default::default() }.write_value(&mut w);
    let (offsets, fds) = objects(&w);
    let r = c3::i_composer_client::dispatch(&svc, call(5, w.data, offsets, fds));
    // status, count, then two unions: marker, tag 5, the parcelable (marker, size, display, result).
    assert_eq!(words(&r.data), [0, 2, 1, 5, 1, 16, 7, 0, 0, 1, 5, 1, 16, 9, 0, 0]);
}

#[test]
fn malformed_requests_are_illegal_arguments_never_panics() {
    let svc = FakeClient::default();
    // Truncated: the config and attribute are missing.
    let r = client_call(&svc, 9, Raw::default().token(CLIENT).i64(0));
    assert_eq!(exception(&r).0, -3);
    // Another interface's token.
    let r = client_call(&svc, 9, Raw::default().token(CALLBACK).i64(0).i32(0).i32(1));
    assert_eq!(exception(&r).0, -3);
    // A token cut short.
    let r = client_call(&svc, 9, Raw::default().i32(i32::MIN).i32(-1));
    assert_eq!(exception(&r).0, -3);
    // executeCommands with a count far past the parcel.
    let r = client_call(&svc, 5, Raw::default().token(CLIENT).i32(0x7fff_ffff));
    assert_eq!(exception(&r).0, -3);

    // Every code, fed garbage after a valid token: an answer, never a panic.
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for code in (1..=50).chain([0x00ff_fffe, 0x00ff_ffff]) {
        for _ in 0..200 {
            let mut req = Raw::default().token(CLIENT);
            let len = (next() % 96) as usize;
            for _ in 0..len {
                // Mostly small words, so lengths and markers are plausible.
                let v = next();
                req = req.i32(if v & 1 == 0 { (v >> 8) as i32 % 4 } else { (v >> 8) as i32 });
            }
            let offsets: Vec<u64> = (0..(next() % 3)).map(|_| (next() % (req.0.len() as u64 + 8)) & !3).collect();
            let fds = vec![file("x")];
            let reply = c3::i_composer_client::dispatch(&svc, call(code, req.0, offsets, fds));
            if !reply.data.is_empty() {
                let status = i32::from_le_bytes(reply.data[..4].try_into().unwrap());
                assert!([0, -3, -7, -8].contains(&status), "code {code}: status {status}");
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// A host service returning an interface; the proxy's encoding

struct FakeComposer;

impl c3::IComposerServer for FakeComposer {
    fn create_client(&self, _: &Ctx<'_>) -> Result<Binder, Status> {
        Ok(Binder::Host(0x1234))
    }

    fn get_capabilities(&self, _: &Ctx<'_>) -> Result<Vec<c3::Capability>, Status> {
        Ok(vec![c3::Capability::BOOT_DISPLAY_CONFIG, c3::Capability::LAYER_LIFECYCLE_BATCH_COMMAND])
    }
}

#[test]
fn an_interface_is_returned_as_a_vintf_binder_object() {
    let data = Raw::default().token("android.hardware.graphics.composer3.IComposer").0;
    let r = c3::i_composer::dispatch(&FakeComposer, call(1, data.clone(), vec![], vec![]));
    assert_eq!(r.binders, [4], "the binder object right after the status");
    let w = words(&r.data);
    assert_eq!(w[0], 0);
    assert_eq!(w[1] as u32, TYPE_BINDER);
    assert_eq!(u64::from_le_bytes(r.data[12..20].try_into().unwrap()), 0x1234);
    assert_eq!(w[7], STABILITY_VINTF);
    let r = c3::i_composer::dispatch(&FakeComposer, call(2, data, vec![], vec![]));
    assert_eq!(words(&r.data), [0, 2, 5, 8]);
    // Served on a broker, it is a host service like any other.
    let ptr = c3::i_composer::serve(&broker(Context::Binder), Arc::new(FakeComposer));
    assert_ne!(ptr, 0);
}

#[test]
fn the_proxy_encodes_requests_and_decodes_replies() {
    use c3::i_composer_callback as cb;
    // onVsync (code 4, oneway): the token as the host writes it, then the arguments.
    assert_eq!(cb::TRANSACTION_ON_VSYNC, 4);
    let w = cb::encode_on_vsync(1, 1_000_000_007, 16_666_666);
    assert_eq!(w.data, Raw::default().token(CALLBACK).i64(1).i64(1_000_000_007).i32(16_666_666).0);
    assert!(w.binders.is_empty() && w.fds.is_empty());
    // onHotplugEvent (code 8): an int-backed enum.
    let w = cb::encode_on_hotplug_event(0, common::DisplayHotplugEvent::ERROR_UNKNOWN);
    assert_eq!(w.data, Raw::default().token(CALLBACK).i64(0).i32(-1).0);
    // onVsyncPeriodTimingChanged: a parcelable argument, marked non-null.
    let t = c3::VsyncPeriodChangeTimeline { new_vsync_applied_time_nanos: 5, refresh_required: true, refresh_time_nanos: 6 };
    let w = cb::encode_on_vsync_period_timing_changed(2, &t);
    assert_eq!(w.data, Raw::default().token(CALLBACK).i64(2).i32(1).i32(24).i64(5).i32(1).i64(6).0);

    // onHotplug is two-way: its reply is a status.
    assert_eq!(cb::decode_on_hotplug(&Raw::default().i32(0).0), Ok(()));
    let err = Raw::default().i32(-8).s16("gone").i32(0).i32(3);
    assert_eq!(cb::decode_on_hotplug(&err.0), Err(CallError::Status(Status::ServiceSpecific(3))));
    let err = Raw::default().i32(-3).s16("bad").i32(0);
    assert_eq!(cb::decode_on_hotplug(&err.0), Err(CallError::Status(Status::Exception(-3, "bad".into()))));
    assert!(matches!(cb::decode_on_hotplug(&[]), Err(CallError::Malformed(_))));

    // A client proxy's decode: createLayer's long, getDisplayConfigs' int array.
    assert_eq!(c3::i_composer_client::decode_create_layer(&Raw::default().i32(0).i64(1 << 33).0), Ok(1 << 33));
    assert_eq!(c3::i_composer_client::decode_get_display_configs(&Raw::default().i32(0).i32(2).i32(0).i32(1).0), Ok(vec![0, 1]));
}
