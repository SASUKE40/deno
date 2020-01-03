// Copyright 2018-2020 the Deno authors. All rights reserved. MIT license.
#![allow(unused)]

use rusty_v8 as v8;
use v8::InIsolate;

use libc::c_char;
use libc::c_int;
use libc::c_void;
use libc::size_t;
use std::collections::HashMap;
use std::convert::From;
use std::convert::TryFrom;
use std::convert::TryInto;
use std::ffi::CString;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::option::Option;
use std::ptr::null;
use std::ptr::NonNull;
use std::slice;

pub type OpId = u32;

#[allow(non_camel_case_types)]
pub type isolate = DenoIsolate;

struct ModuleInfo {
  main: bool,
  name: String,
  handle: v8::Global<v8::Module>,
  import_specifiers: Vec<String>,
}

#[repr(C)]
struct UserDataScope {
  deno_: *mut DenoIsolate,
  prev_data_: *mut c_void,
  data_: *mut c_void,
}

impl UserDataScope {
  pub fn new(deno_ptr: *mut DenoIsolate, data: *mut c_void) -> Self {
    let mut deno = unsafe { &mut (*deno_ptr) };
    assert!(deno.user_data_.is_null() || deno.user_data_ == data);
    let s = Self {
      deno_: deno_ptr,
      data_: data,
      prev_data_: deno.user_data_,
    };
    deno.user_data_ = data;
    s
  }
}

impl Drop for UserDataScope {
  fn drop(&mut self) {
    let mut deno = unsafe { &mut (*self.deno_) };
    assert!(deno.user_data_ == self.data_);
    deno.user_data_ = self.prev_data_;
  }
}

pub struct DenoIsolate {
  isolate_: Option<v8::OwnedIsolate>,
  last_exception_: Option<String>,
  context_: v8::Global<v8::Context>,
  mods_: HashMap<deno_mod, ModuleInfo>,
  mods_by_name_: HashMap<String, deno_mod>,
  locker_: Option<v8::Locker>,
  shared_: deno_buf,
  shared_ab_: v8::Global<v8::SharedArrayBuffer>,
  resolve_cb_: Option<deno_resolve_cb>,
  recv_: v8::Global<v8::Function>,
  user_data_: *mut c_void,
  current_args_: *const v8::FunctionCallbackInfo,
  recv_cb_: deno_recv_cb,
  snapshot_creator_: Option<v8::SnapshotCreator>,
  has_snapshotted_: bool,
  snapshot_: Option<v8::OwnedStartupData>,
  /*
  void* global_import_buf_ptr_;

  deno_dyn_import_id next_dyn_import_id_;
  deno_dyn_import_cb dyn_import_cb_;
  std::map<deno_dyn_import_id, v8::Persistent<v8::Promise::Resolver>>
      dyn_import_map_;

  std::map<int, v8::Persistent<v8::Value>> pending_promise_map_;
  v8::Persistent<v8::Value> last_exception_handle_;

  v8::Persistent<v8::ArrayBuffer> global_import_buf_;
  */
}

impl Drop for DenoIsolate {
  fn drop(&mut self) {
    // TODO Too much boiler plate.
    // <Boilerplate>
    let mut isolate = self.isolate_.take().unwrap();
    {
      let mut locker = v8::Locker::new(&isolate);
      let mut hs = v8::HandleScope::new(&mut locker);
      let scope = hs.enter();
      // </Boilerplate>
      self.context_.reset(scope);
      self.shared_ab_.reset(scope);
      self.recv_.reset(scope);
      for (key, module) in self.mods_.iter_mut() {
        module.handle.reset(scope);
      }
    }
    if let Some(locker_) = self.locker_.take() {
      drop(locker_);
    }
    if let Some(creator) = self.snapshot_creator_.take() {
      // TODO(ry) V8 has a strange assert which prevents a SnapshotCreator from
      // being deallocated if it hasn't created a snapshot yet.
      // https://github.com/v8/v8/blob/73212783fbd534fac76cc4b66aac899c13f71fc8/src/api.cc#L603
      // If that assert is removed, this if guard could be removed.
      // WARNING: There may be false positive LSAN errors here.
      std::mem::forget(isolate);
      if self.has_snapshotted_ {
        drop(creator);
      }
    } else {
      drop(isolate);
    }
  }
}

impl DenoIsolate {
  pub fn new(config: deno_config) -> Self {
    Self {
      isolate_: None,
      last_exception_: None,
      context_: v8::Global::<v8::Context>::new(),
      mods_: HashMap::new(),
      mods_by_name_: HashMap::new(),
      locker_: None,
      shared_: config.shared,
      shared_ab_: v8::Global::<v8::SharedArrayBuffer>::new(),
      resolve_cb_: None,
      recv_: v8::Global::<v8::Function>::new(),
      user_data_: std::ptr::null_mut(),
      current_args_: std::ptr::null(),
      recv_cb_: config.recv_cb,
      snapshot_creator_: None,
      snapshot_: config.load_snapshot,
      has_snapshotted_: false,
    }
  }

  pub fn add_isolate(&mut self, mut isolate: v8::OwnedIsolate) {
    isolate.set_capture_stack_trace_for_uncaught_exceptions(true, 10);
    isolate.set_promise_reject_callback(promise_reject_callback);
    isolate.add_message_listener(message_callback);
    isolate.set_host_initialize_import_meta_object_callback(
      host_initialize_import_meta_object_callback,
    );
    isolate.set_host_import_module_dynamically_callback(
      host_import_module_dynamically_callback,
    );
    let self_ptr: *mut Self = self;
    unsafe { isolate.set_data(0, self_ptr as *mut c_void) };
    self.isolate_ = Some(isolate);
  }

  pub fn register_module(
    &mut self,
    main: bool,
    name: &str,
    source: &str,
  ) -> deno_mod {
    let isolate = self.isolate_.as_ref().unwrap();
    let mut locker = v8::Locker::new(&isolate);

    let mut hs = v8::HandleScope::new(&mut locker);
    let scope = hs.enter();
    let mut context = v8::Context::new(scope);
    context.enter();

    let name_str = v8::String::new(scope, name).unwrap();
    let source_str = v8::String::new(scope, source).unwrap();

    let origin = module_origin(scope, name_str);
    let source = v8::script_compiler::Source::new(source_str, &origin);

    let mut try_catch = v8::TryCatch::new(scope);
    let tc = try_catch.enter();

    let mut maybe_module =
      v8::script_compiler::compile_module(&isolate, source);

    if tc.has_caught() {
      assert!(maybe_module.is_none());
      self.handle_exception(scope, context, tc.exception().unwrap());
      return 0;
    }
    let module = maybe_module.unwrap();
    let id = module.get_identity_hash();

    let mut import_specifiers: Vec<String> = vec![];
    for i in 0..module.get_module_requests_length() {
      let specifier = module.get_module_request(i);
      import_specifiers.push(specifier.to_rust_string_lossy(scope));
    }

    let mut handle = v8::Global::<v8::Module>::new();
    handle.set(scope, module);
    self.mods_.insert(
      id,
      ModuleInfo {
        main,
        name: name.to_string(),
        import_specifiers,
        handle,
      },
    );
    self.mods_by_name_.insert(name.to_string(), id);
    /*
    mods_.emplace(
        std::piecewise_construct, std::make_tuple(id),
        std::make_tuple(isolate_, module, main, name, import_specifiers));
    mods_by_name_[name] = id;
    */

    context.exit();

    id
  }

  fn get_module_info(&self, id: deno_mod) -> Option<&ModuleInfo> {
    if id == 0 {
      return None;
    }
    self.mods_.get(&id)
  }

  // deno::Execute
  fn execute<'a>(
    &mut self,
    s: &mut impl v8::ToLocal<'a>,
    mut context: v8::Local<'a, v8::Context>,
    js_filename: &str,
    js_source: &str,
  ) -> bool {
    let mut hs = v8::HandleScope::new(s);
    let s = hs.enter();

    let source = v8::String::new(s, js_source).unwrap();
    let name = v8::String::new(s, js_filename).unwrap();
    /*
    auto* isolate = context->GetIsolate();
    v8::Isolate::Scope isolate_scope(isolate);
    v8::HandleScope handle_scope(isolate);
    v8::Context::Scope context_scope(context);

    auto source = v8_str(js_source);
    auto name = v8_str(js_filename);

    v8::TryCatch try_catch(isolate);
    */
    let mut try_catch = v8::TryCatch::new(s);
    let tc = try_catch.enter();

    let origin = script_origin(s, name);
    let mut script =
      v8::Script::compile(s, context, source, Some(&origin)).unwrap();
    let result = script.run(s, context);
    /*

    v8::ScriptOrigin origin(name);

    auto script = v8::Script::Compile(context, source, &origin);

    if (script.IsEmpty()) {
      DCHECK(try_catch.HasCaught());
      HandleException(context, try_catch.Exception());
      return false;
    }

    auto result = script.ToLocalChecked()->Run(context);
    */

    if result.is_none() {
      assert!(tc.has_caught());
      let exception = tc.exception().unwrap();
      self.handle_exception(s, context, exception);
      false
    } else {
      true
    }
  }

  fn handle_exception<'a>(
    &mut self,
    s: &mut impl v8::ToLocal<'a>,
    mut context: v8::Local<'a, v8::Context>,
    exception: v8::Local<'a, v8::Value>,
  ) {
    let isolate = context.get_isolate();
    // TerminateExecution was called
    if isolate.is_execution_terminating() {
      // cancel exception termination so that the exception can be created
      isolate.cancel_terminate_execution();

      // maybe make a new exception object
      let exception = if (exception.is_null_or_undefined()) {
        let exception_str =
          v8::String::new(s, "execution terminated").unwrap().into();
        isolate.enter();
        let e = v8::error(s, exception_str);
        isolate.exit();
        e
      } else {
        exception
      };

      // handle the exception as if it is a regular exception
      self.handle_exception(s, context, exception);

      // re-enable exception termination
      context.get_isolate().terminate_execution();
      return;
    }

    let json_str = self.encode_exception_as_json(s, context, exception);
    self.last_exception_ = Some(json_str);
  }

  fn encode_exception_as_json<'a>(
    &mut self,
    s: &mut impl v8::ToLocal<'a>,
    mut context: v8::Local<'a, v8::Context>,
    exception: v8::Local<'a, v8::Value>,
  ) -> String {
    let message = v8::create_message(s, exception);
    self.encode_message_as_json(s, context, message.into())
    /*
    auto* isolate = context->GetIsolate();
    v8::HandleScope handle_scope(isolate);
    v8::Context::Scope context_scope(context);

    auto message = v8::Exception::CreateMessage(isolate, exception);
    return EncodeMessageAsJSON(context, message);
    */
  }

  fn encode_message_as_json<'a>(
    &mut self,
    s: &mut impl v8::ToLocal<'a>,
    mut context: v8::Local<v8::Context>,
    message: v8::Local<v8::Message>,
  ) -> String {
    let json_obj = self.encode_message_as_object(s, context, message);
    let json_string = v8::json::stringify(context, json_obj.into()).unwrap();
    json_string.to_rust_string_lossy(s)
    /*
    auto json_obj = EncodeMessageAsObject(context, message);
    auto json_string = v8::JSON::Stringify(context, json_obj).ToLocalChecked();
    v8::String::Utf8Value json_string_(isolate, json_string);
    return std::string(ToCString(json_string_));
    */
  }

  fn encode_message_as_object<'a>(
    &mut self,
    s: &mut impl v8::ToLocal<'a>,
    mut context: v8::Local<v8::Context>,
    message: v8::Local<v8::Message>,
  ) -> v8::Local<'a, v8::Object> {
    /*
    auto* isolate = context->GetIsolate();
    v8::EscapableHandleScope handle_scope(isolate);
    v8::Context::Scope context_scope(context);
    */
    let json_obj = v8::Object::new(s);

    let exception_str = message.get(s);
    json_obj.set(
      context,
      v8::String::new(s, "message").unwrap().into(),
      exception_str.into(),
    );

    let script_resource_name = message
      .get_script_resource_name(s)
      .expect("Missing ScriptResourceName");
    json_obj.set(
      context,
      v8::String::new(s, "scriptResourceName").unwrap().into(),
      script_resource_name.into(),
    );

    let source_line = message
      .get_source_line(s, context)
      .expect("Missing SourceLine");
    json_obj.set(
      context,
      v8::String::new(s, "sourceLine").unwrap().into(),
      source_line.into(),
    );

    let line_number = message
      .get_line_number(context)
      .expect("Missing LineNumber");
    json_obj.set(
      context,
      v8::String::new(s, "lineNumber").unwrap().into(),
      v8::Integer::new(s, line_number as i32).into(),
    );

    json_obj.set(
      context,
      v8::String::new(s, "startPosition").unwrap().into(),
      v8::Integer::new(s, message.get_start_position() as i32).into(),
    );

    json_obj.set(
      context,
      v8::String::new(s, "endPosition").unwrap().into(),
      v8::Integer::new(s, message.get_end_position() as i32).into(),
    );

    json_obj.set(
      context,
      v8::String::new(s, "errorLevel").unwrap().into(),
      v8::Integer::new(s, message.error_level() as i32).into(),
    );

    json_obj.set(
      context,
      v8::String::new(s, "startColumn").unwrap().into(),
      v8::Integer::new(s, message.get_start_column() as i32).into(),
    );

    json_obj.set(
      context,
      v8::String::new(s, "endColumn").unwrap().into(),
      v8::Integer::new(s, message.get_end_column() as i32).into(),
    );

    let is_shared_cross_origin = if message.is_shared_cross_origin() {
      v8::new_true(s)
    } else {
      v8::new_false(s)
    };

    json_obj.set(
      context,
      v8::String::new(s, "isSharedCrossOrigin").unwrap().into(),
      is_shared_cross_origin.into(),
    );

    let is_opaque = if message.is_opaque() {
      v8::new_true(s)
    } else {
      v8::new_false(s)
    };

    json_obj.set(
      context,
      v8::String::new(s, "isOpaque").unwrap().into(),
      is_opaque.into(),
    );

    let frames = if let Some(stack_trace) = message.get_stack_trace(s) {
      let count = stack_trace.get_frame_count() as i32;
      let frames = v8::Array::new(s, count);

      for i in 0..count {
        let frame = stack_trace
          .get_frame(s, i as usize)
          .expect("No frame found");
        let frame_obj = v8::Object::new(s);
        frames.set(context, v8::Integer::new(s, i).into(), frame_obj.into());
        frame_obj.set(
          context,
          v8::String::new(s, "line").unwrap().into(),
          v8::Integer::new(s, frame.get_line_number() as i32).into(),
        );
        frame_obj.set(
          context,
          v8::String::new(s, "column").unwrap().into(),
          v8::Integer::new(s, frame.get_column() as i32).into(),
        );

        if let Some(function_name) = frame.get_function_name(s) {
          frame_obj.set(
            context,
            v8::String::new(s, "functionName").unwrap().into(),
            function_name.into(),
          );
        }

        let script_name = match frame.get_script_name_or_source_url(s) {
          Some(name) => name,
          None => v8::String::new(s, "<unknown>").unwrap(),
        };
        frame_obj.set(
          context,
          v8::String::new(s, "scriptName").unwrap().into(),
          script_name.into(),
        );

        let is_eval = if frame.is_eval() {
          v8::new_true(s)
        } else {
          v8::new_false(s)
        };
        frame_obj.set(
          context,
          v8::String::new(s, "isEval").unwrap().into(),
          is_eval.into(),
        );

        let is_constructor = if frame.is_constructor() {
          v8::new_true(s)
        } else {
          v8::new_false(s)
        };
        frame_obj.set(
          context,
          v8::String::new(s, "isConstructor").unwrap().into(),
          is_constructor.into(),
        );

        let is_wasm = if frame.is_wasm() {
          v8::new_true(s)
        } else {
          v8::new_false(s)
        };
        frame_obj.set(
          context,
          v8::String::new(s, "isWasm").unwrap().into(),
          is_wasm.into(),
        );
      }

      frames
    } else {
      // No stack trace. We only have one stack frame of info..
      let frames = v8::Array::new(s, 1);
      let frame_obj = v8::Object::new(s);
      frames.set(context, v8::Integer::new(s, 0).into(), frame_obj.into());

      frame_obj.set(
        context,
        v8::String::new(s, "scriptResourceName").unwrap().into(),
        script_resource_name.into(),
      );
      frame_obj.set(
        context,
        v8::String::new(s, "line").unwrap().into(),
        v8::Integer::new(s, line_number as i32).into(),
      );
      frame_obj.set(
        context,
        v8::String::new(s, "column").unwrap().into(),
        v8::Integer::new(s, message.get_start_column() as i32).into(),
      );

      frames
    };

    json_obj.set(
      context,
      v8::String::new(s, "frames").unwrap().into(),
      frames.into(),
    );

    json_obj
  }
}

fn script_origin<'a>(
  s: &mut impl v8::ToLocal<'a>,
  resource_name: v8::Local<'a, v8::String>,
) -> v8::ScriptOrigin<'a> {
  let resource_line_offset = v8::Integer::new(s, 0);
  let resource_column_offset = v8::Integer::new(s, 0);
  let resource_is_shared_cross_origin = v8::new_false(s);
  let script_id = v8::Integer::new(s, 123);
  let source_map_url = v8::String::new(s, "source_map_url").unwrap();
  let resource_is_opaque = v8::new_true(s);
  let is_wasm = v8::new_false(s);
  let is_module = v8::new_false(s);
  v8::ScriptOrigin::new(
    resource_name.into(),
    resource_line_offset,
    resource_column_offset,
    resource_is_shared_cross_origin,
    script_id,
    source_map_url.into(),
    resource_is_opaque,
    is_wasm,
    is_module,
  )
}

fn module_origin<'a>(
  s: &mut impl v8::ToLocal<'a>,
  resource_name: v8::Local<'a, v8::String>,
) -> v8::ScriptOrigin<'a> {
  let resource_line_offset = v8::Integer::new(s, 0);
  let resource_column_offset = v8::Integer::new(s, 0);
  let resource_is_shared_cross_origin = v8::new_false(s);
  let script_id = v8::Integer::new(s, 123);
  let source_map_url = v8::String::new(s, "source_map_url").unwrap();
  let resource_is_opaque = v8::new_true(s);
  let is_wasm = v8::new_false(s);
  let is_module = v8::new_true(s);
  v8::ScriptOrigin::new(
    resource_name.into(),
    resource_line_offset,
    resource_column_offset,
    resource_is_shared_cross_origin,
    script_id,
    source_map_url.into(),
    resource_is_opaque,
    is_wasm,
    is_module,
  )
}

extern "C" fn host_import_module_dynamically_callback(
  _context: v8::Local<v8::Context>,
  _referrer: v8::Local<v8::ScriptOrModule>,
  _specifier: v8::Local<v8::String>,
) -> *mut v8::Promise {
  todo!()
  /*
  auto* isolate = context->GetIsolate();
  DenoIsolate* d = DenoIsolate::FromIsolate(isolate);
  v8::Isolate::Scope isolate_scope(isolate);
  v8::Context::Scope context_scope(context);
  v8::EscapableHandleScope handle_scope(isolate);

  v8::String::Utf8Value specifier_str(isolate, specifier);

  auto referrer_name = referrer->GetResourceName();
  v8::String::Utf8Value referrer_name_str(isolate, referrer_name);

  // TODO(ry) I'm not sure what HostDefinedOptions is for or if we're ever going
  // to use it. For now we check that it is not used. This check may need to be
  // changed in the future.
  auto host_defined_options = referrer->GetHostDefinedOptions();
  CHECK_EQ(host_defined_options->Length(), 0);

  v8::Local<v8::Promise::Resolver> resolver =
      v8::Promise::Resolver::New(context).ToLocalChecked();

  deno_dyn_import_id import_id = d->next_dyn_import_id_++;

  d->dyn_import_map_.emplace(std::piecewise_construct,
                             std::make_tuple(import_id),
                             std::make_tuple(d->isolate_, resolver));

  d->dyn_import_cb_(d->user_data_, *specifier_str, *referrer_name_str,
                    import_id);

  auto promise = resolver->GetPromise();
  return handle_scope.Escape(promise);
  */
}

extern "C" fn host_initialize_import_meta_object_callback(
  context: v8::Local<v8::Context>,
  module: v8::Local<v8::Module>,
  meta: v8::Local<v8::Object>,
) {
  /*
  auto* isolate = context->GetIsolate();
  DenoIsolate* d = DenoIsolate::FromIsolate(isolate);
  v8::Isolate::Scope isolate_scope(isolate);

  CHECK(!module.IsEmpty());

  deno_mod id = module->GetIdentityHash();
  CHECK_NE(id, 0);

  auto* info = d->GetModuleInfo(id);

  const char* url = info->name.c_str();
  const bool main = info->main;

  meta->CreateDataProperty(context, v8_str("url"), v8_str(url)).ToChecked();
  meta->CreateDataProperty(context, v8_str("main"), v8_bool(main)).ToChecked();
  */
  let mut cbs = v8::CallbackScope::new(context);
  let mut hs = v8::HandleScope::new(cbs.enter());
  let scope = hs.enter();
  let mut isolate = scope.isolate();
  let deno_isolate: &mut DenoIsolate =
    unsafe { &mut *(isolate.get_data(0) as *mut DenoIsolate) };

  let id = module.get_identity_hash();
  assert_ne!(id, 0);

  let info = deno_isolate.get_module_info(id).expect("Module not found");

  let main = if info.main {
    v8::new_true(scope)
  } else {
    v8::new_false(scope)
  };

  meta.create_data_property(
    context,
    v8::String::new(scope, "url").unwrap().into(),
    v8::String::new(scope, &info.name).unwrap().into(),
  );
  meta.create_data_property(
    context,
    v8::String::new(scope, "main").unwrap().into(),
    main.into(),
  );
}

extern "C" fn message_callback(
  _message: v8::Local<v8::Message>,
  _exception: v8::Local<v8::Value>,
) {
  todo!()
  /*
  auto* isolate = message->GetIsolate();
  DenoIsolate* d = static_cast<DenoIsolate*>(isolate->GetData(0));
  v8::HandleScope handle_scope(isolate);
  auto context = d->context_.Get(isolate);
  HandleExceptionMessage(context, message);
  */
}

extern "C" fn promise_reject_callback(
  _promise_reject_message: v8::PromiseRejectMessage,
) {
  todo!()
  /*
  auto* isolate = v8::Isolate::GetCurrent();
  DenoIsolate* d = static_cast<DenoIsolate*>(isolate->GetData(0));
  DCHECK_EQ(d->isolate_, isolate);
  v8::HandleScope handle_scope(d->isolate_);
  auto error = promise_reject_message.GetValue();
  auto context = d->context_.Get(d->isolate_);
  auto promise = promise_reject_message.GetPromise();

  v8::Context::Scope context_scope(context);

  int promise_id = promise->GetIdentityHash();
  switch (promise_reject_message.GetEvent()) {
    case v8::kPromiseRejectWithNoHandler:
      // Insert the error into the pending_promise_map_ using the promise's id
      // as the key.
      d->pending_promise_map_.emplace(std::piecewise_construct,
                                      std::make_tuple(promise_id),
                                      std::make_tuple(d->isolate_, error));
      break;

    case v8::kPromiseHandlerAddedAfterReject:
      d->pending_promise_map_.erase(promise_id);
      break;

    case v8::kPromiseRejectAfterResolved:
      break;

    case v8::kPromiseResolveAfterResolved:
      // Should not warn. See #1272
      break;

    default:
      CHECK(false && "unreachable");
  }
  */
}

/// This type represents a borrowed slice.
#[repr(C)]
pub struct deno_buf {
  data_ptr: *const u8,
  data_len: usize,
}

/// `deno_buf` can not clone, and there is no interior mutability.
/// This type satisfies Send bound.
unsafe impl Send for deno_buf {}

impl deno_buf {
  #[inline]
  pub fn empty() -> Self {
    Self {
      data_ptr: null(),
      data_len: 0,
    }
  }

  #[inline]
  pub unsafe fn from_raw_parts(ptr: *const u8, len: usize) -> Self {
    Self {
      data_ptr: ptr,
      data_len: len,
    }
  }
}

/// Converts Rust &Buf to libdeno `deno_buf`.
impl<'a> From<&'a [u8]> for deno_buf {
  #[inline]
  fn from(x: &'a [u8]) -> Self {
    Self {
      data_ptr: x.as_ref().as_ptr(),
      data_len: x.len(),
    }
  }
}

impl<'a> From<&'a mut [u8]> for deno_buf {
  #[inline]
  fn from(x: &'a mut [u8]) -> Self {
    Self {
      data_ptr: x.as_ref().as_ptr(),
      data_len: x.len(),
    }
  }
}

impl Deref for deno_buf {
  type Target = [u8];
  #[inline]
  fn deref(&self) -> &[u8] {
    unsafe { std::slice::from_raw_parts(self.data_ptr, self.data_len) }
  }
}

impl AsRef<[u8]> for deno_buf {
  #[inline]
  fn as_ref(&self) -> &[u8] {
    &*self
  }
}

/// A PinnedBuf encapsulates a slice that's been borrowed from a JavaScript
/// ArrayBuffer object. JavaScript objects can normally be garbage collected,
/// but the existence of a PinnedBuf inhibits this until it is dropped. It
/// behaves much like an Arc<[u8]>, although a PinnedBuf currently can't be
/// cloned.
pub struct PinnedBuf {
  data_ptr: NonNull<u8>,
  data_len: usize,
  backing_store: v8::SharedRef<v8::BackingStore>,
}

unsafe impl Send for PinnedBuf {}

impl PinnedBuf {
  pub fn new(view: v8::Local<v8::ArrayBufferView>) -> Self {
    let mut backing_store = view.buffer().unwrap().get_backing_store();
    let backing_store_ptr = backing_store.data() as *mut _ as *mut u8;
    let view_ptr = unsafe { backing_store_ptr.add(view.byte_offset()) };
    let view_len = view.byte_length();
    Self {
      data_ptr: NonNull::new(view_ptr).unwrap(),
      data_len: view_len,
      backing_store,
    }
  }
}

impl Deref for PinnedBuf {
  type Target = [u8];
  fn deref(&self) -> &[u8] {
    unsafe { slice::from_raw_parts(self.data_ptr.as_ptr(), self.data_len) }
  }
}

impl DerefMut for PinnedBuf {
  fn deref_mut(&mut self) -> &mut [u8] {
    unsafe { slice::from_raw_parts_mut(self.data_ptr.as_ptr(), self.data_len) }
  }
}

impl AsRef<[u8]> for PinnedBuf {
  fn as_ref(&self) -> &[u8] {
    &*self
  }
}

impl AsMut<[u8]> for PinnedBuf {
  fn as_mut(&mut self) -> &mut [u8] {
    &mut *self
  }
}

#[repr(C)]
pub struct deno_snapshot<'a> {
  pub data_ptr: *const u8,
  pub data_len: usize,
  _marker: PhantomData<&'a [u8]>,
}

/// `deno_snapshot` can not clone, and there is no interior mutability.
/// This type satisfies Send bound.
unsafe impl Send for deno_snapshot<'_> {}

// TODO(ry) Snapshot1 and Snapshot2 are not very good names and need to be
// reconsidered. The entire snapshotting interface is still under construction.

/// The type returned from deno_snapshot_new. Needs to be dropped.
pub type Snapshot1 = v8::OwnedStartupData;

/// The type created from slice. Used for loading.
pub type Snapshot2<'a> = v8::StartupData<'a>;

#[allow(non_camel_case_types)]
type deno_recv_cb = unsafe fn(
  user_data: *mut c_void,
  op_id: OpId,
  control_buf: deno_buf,
  zero_copy_buf: Option<PinnedBuf>,
);

/// Called when dynamic import is called in JS: import('foo')
/// Embedder must call deno_dyn_import_done() with the specified id and
/// the module.
#[allow(non_camel_case_types)]
type deno_dyn_import_cb = unsafe extern "C" fn(
  user_data: *mut c_void,
  specifier: *const c_char,
  referrer: *const c_char,
  id: deno_dyn_import_id,
);

#[allow(non_camel_case_types)]
pub type deno_mod = i32;

#[allow(non_camel_case_types)]
pub type deno_dyn_import_id = i32;

#[allow(non_camel_case_types)]
type deno_resolve_cb = unsafe extern "C" fn(
  user_data: *mut c_void,
  specifier: *const c_char,
  referrer: deno_mod,
) -> deno_mod;

#[repr(C)]
pub struct deno_config {
  pub will_snapshot: c_int,
  pub load_snapshot: Option<v8::OwnedStartupData>,
  pub shared: deno_buf,
  pub recv_cb: deno_recv_cb,
  pub dyn_import_cb: deno_dyn_import_cb,
}

pub unsafe fn deno_init() {
  let platform = v8::platform::new_default_platform();
  v8::V8::initialize_platform(platform);
  v8::V8::initialize();
  // TODO(ry) This makes WASM compile synchronously. Eventually we should
  // remove this to make it work asynchronously too. But that requires getting
  // PumpMessageLoop and RunMicrotasks setup correctly.
  // See https://github.com/denoland/deno/issues/2544
  let argv = vec![
    "".to_string(),
    "--no-wasm-async-compilation".to_string(),
    "--harmony-top-level-await".to_string(),
  ];
  v8::V8::set_flags_from_command_line(argv);
}

lazy_static! {
  static ref EXTERNAL_REFERENCES: v8::ExternalReferences =
    v8::ExternalReferences::new(&[
      v8::ExternalReference { function: print },
      v8::ExternalReference { function: recv },
      v8::ExternalReference { function: send },
      v8::ExternalReference {
        function: eval_context
      },
      v8::ExternalReference {
        function: error_to_json
      },
      v8::ExternalReference {
        getter: shared_getter
      },
      v8::ExternalReference {
        message: message_callback
      },
      v8::ExternalReference {
        function: queue_microtask
      },
    ]);
}

pub unsafe fn deno_new_snapshotter(config: deno_config) -> *mut isolate {
  assert_ne!(config.will_snapshot, 0);
  // TODO(ry) Support loading snapshots before snapshotting.
  assert!(config.load_snapshot.is_none());
  let mut creator = v8::SnapshotCreator::new(Some(&EXTERNAL_REFERENCES));

  let mut d = Box::new(DenoIsolate::new(config));
  let isolate = creator.get_owned_isolate();

  let mut locker = v8::Locker::new(&isolate);
  {
    let mut hs = v8::HandleScope::new(&mut locker);
    let scope = hs.enter();
    let mut context = v8::Context::new(scope);
    // context.enter();
    d.context_.set(scope, context);
    creator.set_default_context(context);
    initialize_context(scope, context);
    // context.exit();
  }
  d.add_isolate(isolate);

  d.snapshot_creator_ = Some(creator);

  return Box::into_raw(d);
}

extern "C" fn print(info: &v8::FunctionCallbackInfo) {
  /*
  auto* isolate = args.GetIsolate();
  int argsLen = args.Length();
  if (argsLen < 1 || argsLen > 2) {
    ThrowInvalidArgument(isolate);
  }
  v8::HandleScope handle_scope(isolate);
  bool is_err = args.Length() >= 2 ? args[1]->BooleanValue(isolate) : false;
  FILE* file = is_err ? stderr : stdout;
  */
  #[allow(mutable_transmutes)]
  #[allow(clippy::transmute_ptr_to_ptr)]
  let info: &mut v8::FunctionCallbackInfo =
    unsafe { std::mem::transmute(info) };

  let arg_len = info.length();
  assert!(arg_len >= 0 && arg_len <= 2);

  let obj = info.get_argument(0);
  let is_err_arg = info.get_argument(1);

  let mut hs = v8::HandleScope::new(info);
  let scope = hs.enter();

  let mut is_err = false;

  if arg_len == 2 {
    let int_val = is_err_arg
      .integer_value(scope)
      .expect("Unable to convert to integer");
    is_err = int_val != 0;
  };

  /*
  #ifdef _WIN32
    int fd = _fileno(file);
    if (fd < 0) return;

    HANDLE h = reinterpret_cast<HANDLE>(_get_osfhandle(fd));
    if (h == INVALID_HANDLE_VALUE) return;

    DWORD mode;
    if (GetConsoleMode(h, &mode)) {
      // Print to Windows console. Since the Windows API generally doesn't support
      // UTF-8 encoded text, we have to use `WriteConsoleW()` which uses UTF-16.
      v8::String::Value str(isolate, args[0]);
      auto str_len = static_cast<size_t>(str.length());
      auto str_wchars = reinterpret_cast<WCHAR*>(*str);

      // WriteConsoleW has some limit to how many characters can be written at
      // once, which is unspecified but low enough to be encountered in practice.
      // Therefore we break up the write into chunks of 8kb if necessary.
      size_t chunk_start = 0;
      while (chunk_start < str_len) {
        size_t chunk_end = std::min(chunk_start + 8192, str_len);

        // Do not break in the middle of a surrogate pair. Note that `chunk_end`
        // points to the start of the next chunk, so we check whether it contains
        // the second half of a surrogate pair (a.k.a. "low surrogate").
        if (chunk_end < str_len && str_wchars[chunk_end] >= 0xdc00 &&
            str_wchars[chunk_end] <= 0xdfff) {
          --chunk_end;
        }

        // Write to the console.
        DWORD chunk_len = static_cast<DWORD>(chunk_end - chunk_start);
        DWORD _;
        WriteConsoleW(h, &str_wchars[chunk_start], chunk_len, &_, nullptr);

        chunk_start = chunk_end;
      }
      return;
    }
  #endif  // _WIN32

    v8::String::Utf8Value str(isolate, args[0]);
    fwrite(*str, sizeof(**str), str.length(), file);
    fflush(file);
    */

  let mut isolate = scope.isolate();
  let mut try_catch = v8::TryCatch::new(scope);
  let tc = try_catch.enter();
  let str_ = match obj.to_string(scope) {
    Some(s) => s,
    None => v8::String::new(scope, "").unwrap(),
  };

  if is_err {
    eprint!("{}", str_.to_rust_string_lossy(scope));
  } else {
    print!("{}", str_.to_rust_string_lossy(scope));
  }
}

extern "C" fn recv(info: &v8::FunctionCallbackInfo) {
  #[allow(mutable_transmutes)]
  #[allow(clippy::transmute_ptr_to_ptr)]
  let info: &mut v8::FunctionCallbackInfo =
    unsafe { std::mem::transmute(info) };
  assert_eq!(info.length(), 1);
  let mut isolate = info.get_isolate();
  let deno_isolate: &mut DenoIsolate =
    unsafe { &mut *(isolate.get_data(0) as *mut DenoIsolate) };
  let mut locker = v8::Locker::new(&isolate);
  let mut hs = v8::HandleScope::new(&mut locker);
  let scope = hs.enter();

  if !deno_isolate.recv_.is_empty() {
    let msg = v8::String::new(scope, "Deno.core.recv already called.").unwrap();
    isolate.throw_exception(msg.into());
    return;
  }

  let recv_val = info.get_argument(0);
  let recv_fn = unsafe { v8::Local::<v8::Function>::cast(recv_val) };
  deno_isolate.recv_.set(scope, recv_fn);
}

extern "C" fn send(info: &v8::FunctionCallbackInfo) {
  #[allow(mutable_transmutes)]
  #[allow(clippy::transmute_ptr_to_ptr)]
  let info: &mut v8::FunctionCallbackInfo =
    unsafe { std::mem::transmute(info) };

  let mut hs = v8::HandleScope::new(info);
  let scope = hs.enter();
  let mut isolate = scope.isolate();
  let deno_isolate: &mut DenoIsolate =
    unsafe { &mut *(isolate.get_data(0) as *mut DenoIsolate) };
  assert!(!deno_isolate.context_.is_empty());

  let op_id = v8::Local::<v8::Uint32>::try_from(info.get_argument(0))
    .unwrap()
    .value() as u32;

  let mut control: deno_buf = deno_buf::empty();
  let control_arg = info.get_argument(1);

  if control_arg.is_array_buffer_view() {
    let view = v8::Local::<v8::ArrayBufferView>::try_from(control_arg).unwrap();
    let mut backing_store = view.buffer().unwrap().get_backing_store();
    let backing_store_ptr = backing_store.data() as *mut _ as *mut u8;
    let view_ptr = unsafe { backing_store_ptr.add(view.byte_offset()) };
    let view_len = view.byte_length();
    control = unsafe { deno_buf::from_raw_parts(view_ptr, view_len) };
  }

  let zero_copy: Option<PinnedBuf> =
    v8::Local::<v8::ArrayBufferView>::try_from(info.get_argument(2))
      .map(PinnedBuf::new)
      .ok();

  // TODO: what's the point of this again?
  // DCHECK_NULL(d->current_args_);
  // d->current_args_ = &args;
  assert!(deno_isolate.current_args_.is_null());
  deno_isolate.current_args_ = info;

  unsafe {
    (deno_isolate.recv_cb_)(deno_isolate.user_data_, op_id, control, zero_copy);
  }

  if deno_isolate.current_args_.is_null() {
    // This indicates that deno_repond() was called already.
  } else {
    // Asynchronous.
    deno_isolate.current_args_ = null();
  }
}

extern "C" fn eval_context(info: &v8::FunctionCallbackInfo) {
  todo!()
}

extern "C" fn error_to_json(info: &v8::FunctionCallbackInfo) {
  #[allow(mutable_transmutes)]
  #[allow(clippy::transmute_ptr_to_ptr)]
  let info: &mut v8::FunctionCallbackInfo =
    unsafe { std::mem::transmute(info) };
  assert_eq!(info.length(), 1);
  // <Boilerplate>
  let mut isolate = info.get_isolate();
  let deno_isolate: &mut DenoIsolate =
    unsafe { &mut *(isolate.get_data(0) as *mut DenoIsolate) };
  let mut locker = v8::Locker::new(&isolate);
  assert!(!deno_isolate.context_.is_empty());
  let mut hs = v8::HandleScope::new(&mut locker);
  let scope = hs.enter();
  let mut context = deno_isolate.context_.get(scope).unwrap();
  // </Boilerplate>
  let exception = info.get_argument(0);
  let json_string =
    deno_isolate.encode_exception_as_json(scope, context, exception);
  let s = v8::String::new(scope, &json_string).unwrap();
  let mut rv = info.get_return_value();
  rv.set(s.into());
}

extern "C" fn queue_microtask(info: &v8::FunctionCallbackInfo) {
  todo!()
}

extern "C" fn shared_getter(
  name: v8::Local<v8::Name>,
  info: &v8::PropertyCallbackInfo,
) {
  /*
  v8::Isolate* isolate = info.GetIsolate();
  DenoIsolate* d = DenoIsolate::FromIsolate(isolate);
  DCHECK_EQ(d->isolate_, isolate);
  v8::Locker locker(d->isolate_);
  v8::EscapableHandleScope handle_scope(isolate);
  if (d->shared_.data_ptr == nullptr) {
    return;
  }
  v8::Local<v8::SharedArrayBuffer> ab;
  if (d->shared_ab_.IsEmpty()) {
    // Lazily initialize the persistent external ArrayBuffer.
    ab = v8::SharedArrayBuffer::New(isolate, d->shared_.data_ptr,
                                    d->shared_.data_len,
                                    v8::ArrayBufferCreationMode::kExternalized);
    d->shared_ab_.Reset(isolate, ab);
  }
  auto shared_ab = d->shared_ab_.Get(isolate);
  info.GetReturnValue().Set(shared_ab);
  */
  use v8::InIsolate;

  let shared_ab = {
    #[allow(mutable_transmutes)]
    #[allow(clippy::transmute_ptr_to_ptr)]
    let info: &mut v8::PropertyCallbackInfo =
      unsafe { std::mem::transmute(info) };

    let mut hs = v8::EscapableHandleScope::new(info);
    let scope = hs.enter();
    let mut isolate = scope.isolate();
    let deno_isolate: &mut DenoIsolate =
      unsafe { &mut *(isolate.get_data(0) as *mut DenoIsolate) };

    if deno_isolate.shared_.data_ptr.is_null() {
      return;
    }

    // Lazily initialize the persistent external ArrayBuffer.
    if deno_isolate.shared_ab_.is_empty() {
      #[allow(mutable_transmutes)]
      #[allow(clippy::transmute_ptr_to_ptr)]
      let data_ptr: *mut u8 =
        unsafe { std::mem::transmute(deno_isolate.shared_.data_ptr) };
      let ab = unsafe {
        v8::SharedArrayBuffer::new_DEPRECATED(
          scope,
          data_ptr as *mut c_void,
          deno_isolate.shared_.data_len,
        )
      };
      deno_isolate.shared_ab_.set(scope, ab);
    }

    let shared_ab = deno_isolate.shared_ab_.get(scope).unwrap();
    scope.escape(shared_ab)
  };

  let rv = &mut info.get_return_value();
  rv.set(shared_ab.into());
}

fn initialize_context<'a>(
  scope: &mut impl v8::ToLocal<'a>,
  mut context: v8::Local<v8::Context>,
) {
  context.enter();

  let global = context.global(scope);

  let deno_val = v8::Object::new(scope);

  global.set(
    context,
    v8::String::new(scope, "Deno").unwrap().into(),
    deno_val.into(),
  );

  let mut core_val = v8::Object::new(scope);

  deno_val.set(
    context,
    v8::String::new(scope, "core").unwrap().into(),
    core_val.into(),
  );

  let mut print_tmpl = v8::FunctionTemplate::new(scope, print);
  let mut print_val = print_tmpl.get_function(scope, context).unwrap();
  core_val.set(
    context,
    v8::String::new(scope, "print").unwrap().into(),
    print_val.into(),
  );

  let mut recv_tmpl = v8::FunctionTemplate::new(scope, recv);
  let mut recv_val = recv_tmpl.get_function(scope, context).unwrap();
  core_val.set(
    context,
    v8::String::new(scope, "recv").unwrap().into(),
    recv_val.into(),
  );

  let mut send_tmpl = v8::FunctionTemplate::new(scope, send);
  let mut send_val = send_tmpl.get_function(scope, context).unwrap();
  core_val.set(
    context,
    v8::String::new(scope, "send").unwrap().into(),
    send_val.into(),
  );

  let mut eval_context_tmpl = v8::FunctionTemplate::new(scope, eval_context);
  let mut eval_context_val =
    eval_context_tmpl.get_function(scope, context).unwrap();
  core_val.set(
    context,
    v8::String::new(scope, "evalContext").unwrap().into(),
    eval_context_val.into(),
  );

  let mut error_to_json_tmpl = v8::FunctionTemplate::new(scope, error_to_json);
  let mut error_to_json_val =
    error_to_json_tmpl.get_function(scope, context).unwrap();
  core_val.set(
    context,
    v8::String::new(scope, "errorToJSON").unwrap().into(),
    error_to_json_val.into(),
  );

  core_val.set_accessor(
    context,
    v8::String::new(scope, "shared").unwrap().into(),
    shared_getter,
  );

  // Direct bindings on `window`.
  let mut queue_microtask_tmpl =
    v8::FunctionTemplate::new(scope, queue_microtask);
  let mut queue_microtask_val =
    queue_microtask_tmpl.get_function(scope, context).unwrap();
  global.set(
    context,
    v8::String::new(scope, "queueMicrotask").unwrap().into(),
    queue_microtask_val.into(),
  );

  context.exit();
}

pub unsafe fn deno_new(config: deno_config) -> *mut isolate {
  if config.will_snapshot != 0 {
    return deno_new_snapshotter(config);
  }

  let load_snapshot_is_null = config.load_snapshot.is_none();

  let mut d = Box::new(DenoIsolate::new(config));
  let mut params = v8::Isolate::create_params();
  params.set_array_buffer_allocator(v8::new_default_allocator());
  params.set_external_references(&EXTERNAL_REFERENCES);
  if let Some(ref mut snapshot) = d.snapshot_ {
    params.set_snapshot_blob(snapshot);
  }

  let isolate = v8::Isolate::new(params);
  d.add_isolate(isolate);

  let mut locker = v8::Locker::new(d.isolate_.as_ref().unwrap());
  {
    let mut hs = v8::HandleScope::new(&mut locker);
    let scope = hs.enter();
    let mut context = v8::Context::new(scope);

    if load_snapshot_is_null {
      // If no snapshot is provided, we initialize the context with empty
      // main source code and source maps.
      initialize_context(scope, context);
    }
    d.context_.set(scope, context);
  }
  return Box::into_raw(d);
}

pub unsafe fn deno_delete(i: *mut DenoIsolate) {
  let deno_isolate = unsafe { Box::from_raw(i as *mut DenoIsolate) };
  drop(deno_isolate);
}

pub unsafe fn deno_last_exception(i: *mut DenoIsolate) -> Option<String> {
  (*i).last_exception_.clone()
}

pub unsafe fn deno_clear_last_exception(i: *mut DenoIsolate) {
  let i_mut: &mut DenoIsolate = unsafe { std::mem::transmute(i) };
  i_mut.last_exception_ = None;
}

pub unsafe fn deno_check_promise_errors(d: *mut DenoIsolate) {
  /*
  if (d->pending_promise_map_.size() > 0) {
    auto* isolate = d->isolate_;
    v8::Locker locker(isolate);
    v8::Isolate::Scope isolate_scope(isolate);
    v8::HandleScope handle_scope(isolate);
    auto context = d->context_.Get(d->isolate_);
    v8::Context::Scope context_scope(context);

    auto it = d->pending_promise_map_.begin();
    while (it != d->pending_promise_map_.end()) {
      auto error = it->second.Get(isolate);
      deno::HandleException(context, error);
      it = d->pending_promise_map_.erase(it);
    }
  }
  */
}

pub unsafe fn deno_lock(i: *mut DenoIsolate) {
  let i_mut: &mut DenoIsolate = unsafe { std::mem::transmute(i) };
  assert!(i_mut.locker_.is_none());
  let mut locker = v8::Locker::new(i_mut.isolate_.as_ref().unwrap());
  i_mut.locker_ = Some(locker);
}

pub unsafe fn deno_unlock(i: *mut DenoIsolate) {
  let i_mut: &mut DenoIsolate = unsafe { std::mem::transmute(i) };
  i_mut.locker_.take().unwrap();
}

pub unsafe fn deno_throw_exception(i: *mut DenoIsolate, text: &str) {
  let i_mut: &mut DenoIsolate = unsafe { std::mem::transmute(i) };
  let isolate = i_mut.isolate_.as_ref().unwrap();
  let mut locker = v8::Locker::new(isolate);
  let mut hs = v8::HandleScope::new(&mut locker);
  let scope = hs.enter();
  let msg = v8::String::new(scope, text).unwrap();
  isolate.throw_exception(msg.into());
}

pub unsafe fn deno_import_buf<'sc>(
  scope: &mut impl v8::ToLocal<'sc>,
  buf: deno_buf,
) -> v8::Local<'sc, v8::Uint8Array> {
  /*
  if (buf.data_ptr == nullptr) {
    return v8::Local<v8::Uint8Array>();
  }
  */

  if buf.data_ptr.is_null() {
    let mut ab = v8::ArrayBuffer::new(scope, 0);
    return v8::Uint8Array::new(ab, 0, 0).expect("Failed to create UintArray8");
  }

  /*
  // To avoid excessively allocating new ArrayBuffers, we try to reuse a single
  // global ArrayBuffer. The caveat is that users must extract data from it
  // before the next tick. We only do this for ArrayBuffers less than 1024
  // bytes.
  v8::Local<v8::ArrayBuffer> ab;
  void* data;
  if (buf.data_len > GLOBAL_IMPORT_BUF_SIZE) {
    // Simple case. We allocate a new ArrayBuffer for this.
    ab = v8::ArrayBuffer::New(d->isolate_, buf.data_len);
    data = ab->GetBackingStore()->Data();
  } else {
    // Fast case. We reuse the global ArrayBuffer.
    if (d->global_import_buf_.IsEmpty()) {
      // Lazily initialize it.
      DCHECK_NULL(d->global_import_buf_ptr_);
      ab = v8::ArrayBuffer::New(d->isolate_, GLOBAL_IMPORT_BUF_SIZE);
      d->global_import_buf_.Reset(d->isolate_, ab);
      d->global_import_buf_ptr_ = ab->GetBackingStore()->Data();
    } else {
      DCHECK(d->global_import_buf_ptr_);
      ab = d->global_import_buf_.Get(d->isolate_);
    }
    data = d->global_import_buf_ptr_;
  }
  memcpy(data, buf.data_ptr, buf.data_len);
  auto view = v8::Uint8Array::New(ab, 0, buf.data_len);
  return view;
  */

  // TODO(bartlomieju): for now skipping part with `global_import_buf_`
  // and always creating new buffer
  let mut ab = v8::ArrayBuffer::new(scope, buf.data_len);
  let mut backing_store = ab.get_backing_store();
  let data = backing_store.data();
  let data: *mut u8 = unsafe { std::mem::transmute(data) };
  std::ptr::copy_nonoverlapping(buf.data_ptr, data, buf.data_len);
  return v8::Uint8Array::new(ab, 0, buf.data_len)
    .expect("Failed to create UintArray8");
}

pub unsafe fn deno_respond(
  i: *mut isolate,
  user_data: *const c_void,
  op_id: OpId,
  buf: deno_buf,
) {
  /*
  auto* d = deno::unwrap(d_);
  if (d->current_args_ != nullptr) {
    // Synchronous response.
    // Note op_id is not passed back in the case of synchronous response.
    if (buf.data_ptr != nullptr && buf.data_len > 0) {
      auto ab = deno::ImportBuf(d, buf);
      d->current_args_->GetReturnValue().Set(ab);
    }
    d->current_args_ = nullptr;
    return;
  }
  */
  let deno_isolate: &mut DenoIsolate = unsafe { std::mem::transmute(i) };

  if !deno_isolate.current_args_.is_null() {
    // Synchronous response.
    // Note op_id is not passed back in the case of synchronous response.
    if !buf.data_ptr.is_null() && buf.data_len > 0 {
      let isolate = deno_isolate.isolate_.as_ref().unwrap();
      let mut locker = v8::Locker::new(isolate);
      assert!(!deno_isolate.context_.is_empty());
      let mut hs = v8::HandleScope::new(&mut locker);
      let scope = hs.enter();
      let ab = deno_import_buf(scope, buf);
      let info: &mut v8::FunctionCallbackInfo =
        unsafe { std::mem::transmute(deno_isolate.current_args_) };
      let rv = &mut info.get_return_value();
      rv.set(ab.into())
    }
    deno_isolate.current_args_ = std::ptr::null();
    return;
  }

  /*
  // Asynchronous response.
  deno::UserDataScope user_data_scope(d, user_data);
  v8::Isolate::Scope isolate_scope(d->isolate_);
  v8::HandleScope handle_scope(d->isolate_);

  auto context = d->context_.Get(d->isolate_);
  v8::Context::Scope context_scope(context);

  v8::TryCatch try_catch(d->isolate_);

  auto recv_ = d->recv_.Get(d->isolate_);
  if (recv_.IsEmpty()) {
    d->last_exception_ = "Deno.core.recv has not been called.";
    return;
  }

  v8::Local<v8::Value> args[2];
  int argc = 0;

  if (buf.data_ptr != nullptr) {
    args[0] = v8::Integer::New(d->isolate_, op_id);
    args[1] = deno::ImportBuf(d, buf);
    argc = 2;
  }

  auto v = recv_->Call(context, context->Global(), argc, args);

  if (try_catch.HasCaught()) {
    CHECK(v.IsEmpty());
    deno::HandleException(context, try_catch.Exception());
  }
  */

  let user_data: *mut c_void = unsafe { std::mem::transmute(user_data) };
  let user_scope = UserDataScope::new(deno_isolate, user_data);
  let isolate = deno_isolate.isolate_.as_ref().unwrap();
  // println!("deno_execute -> Isolate ptr {:?}", isolate);
  let mut locker = v8::Locker::new(isolate);
  assert!(!deno_isolate.context_.is_empty());
  let mut hs = v8::HandleScope::new(&mut locker);
  let scope = hs.enter();
  let mut context = deno_isolate.context_.get(scope).unwrap();
  context.enter();

  let mut try_catch = v8::TryCatch::new(scope);
  let tc = try_catch.enter();

  let recv_ = deno_isolate.recv_.get(scope);

  if recv_.is_none() {
    let msg = "Deno.core.recv has not been called.".to_string();
    deno_isolate.last_exception_ = Some(msg);
    return;
  }

  let mut argc = 0;
  let mut args: Vec<v8::Local<v8::Value>> = vec![];

  if !buf.data_ptr.is_null() {
    argc = 2;
    let op_id = v8::Integer::new(scope, op_id as i32);
    args.push(op_id.into());
    let buf = deno_import_buf(scope, buf);
    args.push(buf.into());
  }

  let global = context.global(scope);
  let maybe_value =
    recv_
      .unwrap()
      .call(scope, context, global.into(), argc, args);

  if tc.has_caught() {
    assert!(maybe_value.is_none());
    deno_isolate.handle_exception(scope, context, tc.exception().unwrap());
  }
  context.exit();
}

pub unsafe fn deno_execute(
  i: *mut DenoIsolate,
  user_data: *mut c_void,
  js_filename: &str,
  js_source: &str,
) {
  let i_mut: &mut DenoIsolate = unsafe { std::mem::transmute(i) };
  let _user_data_scope = UserDataScope::new(i, user_data);
  let isolate = i_mut.isolate_.as_ref().unwrap();
  // println!("deno_execute -> Isolate ptr {:?}", isolate);
  let mut locker = v8::Locker::new(isolate);
  assert!(!i_mut.context_.is_empty());
  let mut hs = v8::HandleScope::new(&mut locker);
  let scope = hs.enter();
  let mut context = i_mut.context_.get(scope).unwrap();
  context.enter();

  i_mut.execute(scope, context, js_filename, js_source);

  context.exit();
  /*
  auto* d = deno::unwrap(d_);
  deno::UserDataScope user_data_scope(d, user_data);
  auto* isolate = d->isolate_;
  v8::Locker locker(isolate);
  v8::Isolate::Scope isolate_scope(isolate);
  v8::HandleScope handle_scope(isolate);
  auto context = d->context_.Get(d->isolate_);
  CHECK(!context.IsEmpty());
  execute(context, js_filename, js_source);
  */
}

pub unsafe fn deno_terminate_execution(i: *mut DenoIsolate) {
  /*
  deno::DenoIsolate* d = reinterpret_cast<deno::DenoIsolate*>(d_);
  d->isolate_->TerminateExecution();
  */
  let i_mut: &mut DenoIsolate = unsafe { std::mem::transmute(i) };
  let isolate = i_mut.isolate_.as_ref().unwrap();
  isolate.terminate_execution();
}

#[allow(dead_code)]
pub unsafe fn deno_run_microtasks(i: *const isolate, user_data: *mut c_void) {
  /*
  deno::DenoIsolate* d = reinterpret_cast<deno::DenoIsolate*>(d_);
  deno::UserDataScope user_data_scope(d, user_data);
  v8::Locker locker(d->isolate_);
  v8::Isolate::Scope isolate_scope(d->isolate_);
  d->isolate_->RunMicrotasks();
  */
  let deno_isolate: &mut DenoIsolate = unsafe { std::mem::transmute(i) };
  let user_scope = UserDataScope::new(deno_isolate, user_data);
  let isolate = deno_isolate.isolate_.as_mut().unwrap();
  let mut locker = v8::Locker::new(isolate);
  isolate.enter();
  isolate.run_microtasks();
  isolate.exit();
}

// Modules

pub unsafe fn deno_mod_new(
  i: *mut DenoIsolate,
  main: bool,
  name: &str,
  source: &str,
) -> deno_mod {
  let i_mut: &mut DenoIsolate = unsafe { std::mem::transmute(i) };
  i_mut.register_module(main, name, source)
}

pub unsafe fn deno_mod_imports_len(i: *mut DenoIsolate, id: deno_mod) -> usize {
  let info = (*i).get_module_info(id).unwrap();
  info.import_specifiers.len()
}

pub unsafe fn deno_mod_imports_get(
  i: *mut DenoIsolate,
  id: deno_mod,
  index: size_t,
) -> Option<String> {
  match (*i).get_module_info(id) {
    Some(info) => match info.import_specifiers.get(index) {
      Some(ref specifier) => Some(specifier.to_string()),
      None => None,
    },
    None => None,
  }
}

fn resolve_callback(
  context: v8::Local<v8::Context>,
  specifier: v8::Local<v8::String>,
  referrer: v8::Local<v8::Module>,
) -> *mut v8::Module {
  use v8::InIsolate;
  /*
  auto* isolate = context->GetIsolate();
  v8::Isolate::Scope isolate_scope(isolate);
  v8::Locker locker(isolate);

  DenoIsolate* d = DenoIsolate::FromIsolate(isolate);

  v8::EscapableHandleScope handle_scope(isolate);
  */

  let mut cbs = v8::CallbackScope::new(context);
  let cb_scope = cbs.enter();
  let isolate = cb_scope.isolate();
  let deno_isolate: &mut DenoIsolate =
    unsafe { &mut *(isolate.get_data(0) as *mut DenoIsolate) };

  let mut locker = v8::Locker::new(isolate);
  let mut hs = v8::EscapableHandleScope::new(&mut locker);
  let scope = hs.enter();

  /*
  deno_mod referrer_id = referrer->GetIdentityHash();
  auto* referrer_info = d->GetModuleInfo(referrer_id);
  CHECK_NOT_NULL(referrer_info);

  for (int i = 0; i < referrer->GetModuleRequestsLength(); i++) {
    Local<String> req = referrer->GetModuleRequest(i);

    if (req->Equals(context, specifier).ToChecked()) {
      v8::String::Utf8Value req_utf8(isolate, req);
      std::string req_str(*req_utf8);

      deno_mod id = d->resolve_cb_(d->user_data_, req_str.c_str(), referrer_id);

      // Note: id might be zero, in which case GetModuleInfo will return
      // nullptr.
      auto* info = d->GetModuleInfo(id);
      if (info == nullptr) {
        char buf[64 * 1024];
        snprintf(buf, sizeof(buf), "Cannot resolve module \"%s\" from \"%s\"",
                 req_str.c_str(), referrer_info->name.c_str());
        isolate->ThrowException(deno::v8_str(buf));
        break;
      } else {
        Local<Module> child_mod = info->handle.Get(isolate);
        return handle_scope.Escape(child_mod);
      }
    }
  }

  return v8::MaybeLocal<v8::Module>();  // Error
  */

  let referrer_id = referrer.get_identity_hash();
  let referrer_info = deno_isolate
    .get_module_info(referrer_id)
    .expect("ModuleInfo not found");
  let len_ = referrer.get_module_requests_length();

  let specifier_str = specifier.to_rust_string_lossy(scope);

  for i in 0..len_ {
    let req = referrer.get_module_request(i);
    let req_str = req.to_rust_string_lossy(scope);

    if req_str == specifier_str {
      let resolve_cb = deno_isolate.resolve_cb_.unwrap();
      let c_str = CString::new(req_str.to_string()).unwrap();
      let c_req_str: *const c_char = c_str.as_ptr() as *const c_char;
      let id =
        unsafe { resolve_cb(deno_isolate.user_data_, c_req_str, referrer_id) };
      let maybe_info = deno_isolate.get_module_info(id);

      if maybe_info.is_none() {
        let msg = format!(
          "Cannot resolve module \"{}\" from \"{}\"",
          req_str, referrer_info.name
        );
        let msg = v8::String::new(scope, &msg).unwrap();
        isolate.throw_exception(msg.into());
        break;
      }

      let child_mod =
        maybe_info.unwrap().handle.get(scope).expect("Empty handle");
      return &mut *scope.escape(child_mod);
    }
  }

  std::ptr::null_mut()
}

pub unsafe fn deno_mod_instantiate(
  i: *mut DenoIsolate,
  user_data: *mut c_void,
  id: deno_mod,
  resolve_cb: deno_resolve_cb,
) {
  /*
  auto* d = deno::unwrap(d_);
  deno::UserDataScope user_data_scope(d, user_data);

  auto* isolate = d->isolate_;
  v8::Isolate::Scope isolate_scope(isolate);
  v8::Locker locker(isolate);
  v8::HandleScope handle_scope(isolate);
  auto context = d->context_.Get(d->isolate_);
  v8::Context::Scope context_scope(context);
  */
  let i_mut: &mut DenoIsolate = unsafe { std::mem::transmute(i) };
  let user_scope = UserDataScope::new(i_mut, user_data);
  let isolate = i_mut.isolate_.as_ref().unwrap();
  let mut locker = v8::Locker::new(isolate);
  let mut hs = v8::HandleScope::new(&mut locker);
  let scope = hs.enter();
  assert!(!i_mut.context_.is_empty());
  let mut context = i_mut.context_.get(scope).unwrap();
  context.enter();

  /*
  v8::TryCatch try_catch(isolate);
  {
    CHECK_NULL(d->resolve_cb_);
    d->resolve_cb_ = cb;
    {
      auto* info = d->GetModuleInfo(id);
      if (info == nullptr) {
        return;
      }
      Local<Module> module = info->handle.Get(isolate);
      if (module->GetStatus() == Module::kErrored) {
        return;
      }
      auto maybe_ok = module->InstantiateModule(context, ResolveCallback);
      CHECK(maybe_ok.IsJust() || try_catch.HasCaught());
    }
    d->resolve_cb_ = nullptr;
  }

  if (try_catch.HasCaught()) {
    HandleException(context, try_catch.Exception());
  }
  */
  let mut try_catch = v8::TryCatch::new(scope);
  let tc = try_catch.enter();

  assert!(i_mut.resolve_cb_.is_none());
  i_mut.resolve_cb_ = Some(resolve_cb);

  let maybe_info = i_mut.get_module_info(id);

  if maybe_info.is_none() {
    return;
  }

  let module_handle = &maybe_info.unwrap().handle;
  let mut module = module_handle.get(scope).unwrap();

  if module.get_status() == v8::ModuleStatus::Errored {
    return;
  }

  let maybe_ok = module.instantiate_module(context, resolve_callback);
  assert!(maybe_ok.is_some() || tc.has_caught());
  i_mut.resolve_cb_.take();

  if tc.has_caught() {
    i_mut.handle_exception(scope, context, tc.exception().unwrap());
  }

  context.exit();
}

pub unsafe fn deno_mod_evaluate(
  i: *mut DenoIsolate,
  user_data: *const c_void,
  id: deno_mod,
) {
  /*
  auto* d = deno::unwrap(d_);
  deno::UserDataScope user_data_scope(d, user_data);

  auto* isolate = d->isolate_;
  v8::Isolate::Scope isolate_scope(isolate);
  v8::Locker locker(isolate);
  v8::HandleScope handle_scope(isolate);
  auto context = d->context_.Get(d->isolate_);
  v8::Context::Scope context_scope(context);

  */
  let deno_isolate: &mut DenoIsolate = unsafe { std::mem::transmute(i) };
  let user_data: *mut c_void = unsafe { std::mem::transmute(user_data) };
  let user_scope = UserDataScope::new(deno_isolate, user_data);
  let isolate = deno_isolate.isolate_.as_ref().unwrap();
  let mut locker = v8::Locker::new(isolate);
  let mut hs = v8::HandleScope::new(&mut locker);
  let scope = hs.enter();
  assert!(!deno_isolate.context_.is_empty());
  let mut context = deno_isolate.context_.get(scope).unwrap();
  context.enter();

  /*
  auto* info = d->GetModuleInfo(id);
  auto module = info->handle.Get(isolate);
  auto status = module->GetStatus();

  if (status == Module::kInstantiated) {
    bool ok = !module->Evaluate(context).IsEmpty();
    status = module->GetStatus();  // Update status after evaluating.
    if (ok) {
      // Note status can still be kErrored even if we get ok.
      CHECK(status == Module::kEvaluated || status == Module::kErrored);
    } else {
      CHECK_EQ(status, Module::kErrored);
    }
  }

  switch (status) {
    case Module::kEvaluated:
      ClearException(context);
      break;
    case Module::kErrored:
      HandleException(context, module->GetException());
      break;
    default:
      FATAL("Unexpected module status: %d", static_cast<int>(status));
  }
  */

  let info = deno_isolate
    .get_module_info(id)
    .expect("ModuleInfo not found");
  let mut module = info.handle.get(scope).expect("Empty module handle");
  let mut status = module.get_status();

  if status == v8::ModuleStatus::Instantiated {
    let ok = module.evaluate(scope, context).is_some();
    // Update status after evaluating.
    status = module.get_status();
    if ok {
      assert!(
        status == v8::ModuleStatus::Evaluated
          || status == v8::ModuleStatus::Errored
      );
    } else {
      assert!(status == v8::ModuleStatus::Errored);
    }
  }

  match status {
    v8::ModuleStatus::Evaluated => {
      // ClearException(context)
      deno_isolate.last_exception_ = None;
    }
    v8::ModuleStatus::Errored => {
      deno_isolate.handle_exception(scope, context, module.get_exception());
    }
    other => panic!("Unexpected module status {:?}", other),
  };

  context.exit();
}

/// Call exactly once for every deno_dyn_import_cb.
pub unsafe fn deno_dyn_import_done(
  i: *mut isolate,
  user_data: *const c_void,
  id: deno_dyn_import_id,
  mod_id: deno_mod,
  error_str: *const c_char,
) {
  todo!()
}

pub fn deno_snapshot_new(i: *mut DenoIsolate) -> v8::OwnedStartupData {
  let deno_isolate: &mut DenoIsolate = unsafe { std::mem::transmute(i) };
  assert!(deno_isolate.snapshot_creator_.is_some());

  let isolate = deno_isolate.isolate_.as_ref().unwrap();
  let mut locker = v8::Locker::new(isolate);
  let mut hs = v8::HandleScope::new(&mut locker);
  let scope = hs.enter();

  // d.clear_modules();
  deno_isolate.context_.reset(scope);

  let snapshot_creator = deno_isolate.snapshot_creator_.as_mut().unwrap();
  let startup_data = snapshot_creator
    .create_blob(v8::FunctionCodeHandling::Keep)
    .unwrap();
  deno_isolate.has_snapshotted_ = true;
  startup_data
}

#[allow(dead_code)]
pub unsafe fn deno_snapshot_delete(s: &mut deno_snapshot) {
  todo!()
}
