//! Android glue. The Kotlin side (android/app/src/main/kotlin/dev/velora/player) owns the
//! foreground service, MediaSession and the media notification; Rust talks to it through the
//! static methods of `MediaBridge` and receives button presses through the `native*` exports below.
//!
//! JNI pitfall handled here: threads created by Rust cannot `FindClass` app classes, so the bridge
//! class is loaded once through the Activity's ClassLoader and cached as a GlobalRef.

use crate::platform::{NativeCmd, NowPlayingInfo, Platform, NATIVE_TX};
use jni::objects::{GlobalRef, JClass, JFloatArray, JObject, JValue};
use jni::sys::{jboolean, jint, jlong, jobject};
use jni::{JNIEnv, JavaVM};
use std::path::PathBuf;
use std::sync::OnceLock;

static BRIDGE: OnceLock<GlobalRef> = OnceLock::new();

fn bridge_class<'a>(env: &mut JNIEnv<'a>, activity: &JObject) -> jni::errors::Result<JClass<'a>> {
    if let Some(g) = BRIDGE.get() {
        let local = env.new_local_ref(g.as_obj())?;
        return Ok(JClass::from(local));
    }
    let loader = env.call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?.l()?;
    let name = env.new_string("dev.velora.player.MediaBridge")?;
    let cls = env
        .call_method(&loader, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", &[JValue::Object(&name)])?
        .l()?;
    let _ = BRIDGE.set(env.new_global_ref(&cls)?);
    Ok(JClass::from(cls))
}

/// Runs `f` with an attached JNIEnv, the Activity and the cached bridge class.
fn with_bridge<R>(f: impl FnOnce(&mut JNIEnv, &JClass) -> jni::errors::Result<R>) -> Option<R> {
    let ctx = ndk_context::android_context();
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    let activity = unsafe { JObject::from_raw(ctx.context() as jobject) };
    let result = (|| {
        let cls = bridge_class(&mut env, &activity)?;
        f(&mut env, &cls)
    })();
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    match result {
        Ok(v) => Some(v),
        Err(e) => {
            log::warn!("jni call failed: {e}");
            None
        }
    }
}

pub struct AndroidPlatform;

impl Platform for AndroidPlatform {
    fn has_audio_permission(&self) -> bool {
        with_bridge(|env, cls| env.call_static_method(cls, "hasAudioPermission", "()Z", &[])?.z()).unwrap_or(false)
    }

    fn request_audio_permission(&self) {
        let _ = with_bridge(|env, cls| env.call_static_method(cls, "requestAudioPermission", "()V", &[]).map(|_| ()));
    }

    fn request_notification_permission(&self) {
        let _ = with_bridge(|env, cls| env.call_static_method(cls, "requestNotificationPermission", "()V", &[]).map(|_| ()));
    }

    fn music_roots(&self) -> Vec<PathBuf> {
        // Primary shared storage + every removable volume (/storage/XXXX-XXXX)
        let mut v = vec![PathBuf::from("/storage/emulated/0")];
        if let Ok(rd) = std::fs::read_dir("/storage") {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name != "emulated" && name != "self" && e.path().is_dir() {
                    v.push(e.path());
                }
            }
        }
        v
    }

    fn update_now_playing(&self, info: &NowPlayingInfo) {
        let _ = with_bridge(|env, cls| {
            let title = env.new_string(&info.title)?;
            let artist = env.new_string(&info.artist)?;
            let album = env.new_string(&info.album)?;
            let cover = match &info.cover_path {
                Some(p) => JObject::from(env.new_string(p)?),
                None => JObject::null(),
            };
            env.call_static_method(
                cls,
                "updateNowPlaying",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;JJZLjava/lang/String;)V",
                &[
                    JValue::Object(&title),
                    JValue::Object(&artist),
                    JValue::Object(&album),
                    JValue::Long(info.duration_ms as jlong),
                    JValue::Long(info.position_ms as jlong),
                    JValue::Bool(info.playing as jboolean),
                    JValue::Object(&cover),
                ],
            )
            .map(|_| ())
        });
    }

    fn stop_playback_service(&self) {
        let _ = with_bridge(|env, cls| env.call_static_method(cls, "stopPlayback", "()V", &[]).map(|_| ()));
    }

    fn insets_dp(&self) -> (f32, f32) {
        with_bridge(|env, cls| {
            let obj = env.call_static_method(cls, "insetsDp", "()[F", &[])?.l()?;
            let arr = JFloatArray::from(obj);
            let mut buf = [0f32; 2];
            env.get_float_array_region(&arr, 0, &mut buf)?;
            Ok((buf[0], buf[1]))
        })
        .unwrap_or((0.0, 0.0))
    }
}

// ---------------------------------------------------------------------------------------
// Exports called by Kotlin. Names must match package dev.velora.player + class MediaBridge.
// ---------------------------------------------------------------------------------------
#[no_mangle]
pub extern "system" fn Java_dev_velora_player_MediaBridge_nativeCommand(_env: JNIEnv, _cls: JClass, cmd: jint, arg: jlong) {
    let Some(tx) = NATIVE_TX.get() else { return };
    let c = match cmd {
        0 => NativeCmd::Play,
        1 => NativeCmd::Pause,
        2 => NativeCmd::Toggle,
        3 => NativeCmd::Next,
        4 => NativeCmd::Prev,
        5 => NativeCmd::SeekMs(arg.max(0) as u64),
        6 => NativeCmd::Stop,
        7 => NativeCmd::Resume,
        8 => NativeCmd::PermissionGranted,
        _ => return,
    };
    let _ = tx.send(c);
}
