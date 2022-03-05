use core::ops::FnOnce;
use std::alloc::{alloc, Layout};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::thread;
use std::thread::ThreadId;

use anyhow::anyhow;
use lazy_static;
use wasmer::{Function, import_namespace, ImportObject, Memory, MemoryView, Module, WasmerEnv};
use wasmer_wasi::WasiState;

use crate::{modules, WasmUri};
use crate::handler::*;
use crate::wasm_file::WasmFile;

lazy_static::lazy_static! {
    static ref INSTANCES: RwLock<HashMap<LocalInstanceKey, Mutex<Box<Instance>>>> = RwLock::new(HashMap::new());
}

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
struct LocalInstanceKey {
    wasm_uri: WasmUri,
    thread_id: ThreadId,
}

impl LocalInstanceKey {
    fn from(wasm_uri: WasmUri) -> LocalInstanceKey {
        LocalInstanceKey { wasm_uri, thread_id: thread::current().id() }
    }
}

#[derive(Clone, WasmerEnv, Debug)]
struct InstanceEnv {
    key: LocalInstanceKey,
    ptr: *mut Instance,
}

unsafe impl Sync for InstanceEnv {}

unsafe impl Send for InstanceEnv {}

impl InstanceEnv {
    fn from(key: LocalInstanceKey) -> Self {
        unsafe {
            let ptr = alloc(Layout::new::<Instance>()) as *mut Instance;
            InstanceEnv {
                ptr,
                key,
            }
        }
    }
    fn init_instance(&self, instance: Instance) {
        unsafe {
            self.ptr.write(instance);
            INSTANCES.write().unwrap().insert(self.key.clone(), Mutex::new(Box::from_raw(self.ptr)));
        }
    }
    fn as_instance(&self) -> &Instance {
        unsafe { &*self.ptr }
    }
}

pub(crate) fn load<B, W>(wasm_file: W) -> Result<WasmUri>
    where B: AsRef<[u8]>,
          W: WasmFile<B>,
{
    let ins = Instance::load_and_new_local(wasm_file)?;
    Ok(ins.key.wasm_uri.clone())
}


pub(crate) fn with<F, R>(wasm_uri: WasmUri, callback: F) -> Result<R>
    where F: FnOnce(&Instance) -> Result<R>
{
    let key = LocalInstanceKey::from(wasm_uri);
    {
        if let Some(ins) = INSTANCES.read().unwrap().get(&key) {
            return callback(&ins.lock().unwrap())
        }
    }
    return callback(Instance::new_local(key)?.as_instance());
}

#[derive(Clone, Debug)]
pub(crate) struct Instance {
    key: LocalInstanceKey,
    instance: wasmer::Instance,
    loaded: Cell<bool>,
    context: RefCell<Context>,
}

#[derive(Clone, Debug)]
pub(crate) struct Context {
    ctx_bytes: Vec<u8>,
    swap_memory: Vec<u8>,
}

impl Context {
    fn with_capacity(capacity: usize) -> Self {
        Self { ctx_bytes: Vec::with_capacity(capacity), swap_memory: Vec::with_capacity(capacity) }
    }

    fn set_args<M: Message>(&mut self, ctx: Option<M>, in_args: InArgs) -> (usize, usize) {
        let args_size = write_to_vec(&in_args, &mut self.swap_memory);
        if args_size == 0 {
            unsafe { self.swap_memory.set_len(0) }
        }
        let ctx_size = if let Some(ctx) = ctx {
            write_to_vec(&ctx, &mut self.ctx_bytes)
        } else {
            unsafe { self.ctx_bytes.set_len(0) };
            0
        };
        (ctx_size, args_size)
    }

    fn out_rets(&mut self) -> OutRets {
        unsafe { self.ctx_bytes.set_len(0) };
        let res = if self.swap_memory.len() > 0 {
            OutRets::parse_from_bytes(self.swap_memory.as_slice()).unwrap()
        } else {
            OutRets::new()
        };
        unsafe { self.swap_memory.set_len(0) };
        res
    }
}

unsafe impl Sync for Instance {}

unsafe impl Send for Instance {}

impl Instance {
    fn new_local(key: LocalInstanceKey) -> anyhow::Result<InstanceEnv> {
        if let Some(module) = modules::MODULES.read().unwrap().get(&key.wasm_uri) {
            Self::from_module(module, key)
        } else {
            Err(anyhow!("not found module"))
        }
    }

    fn load_and_new_local<B, W>(wasm_file: W) -> anyhow::Result<InstanceEnv>
        where B: AsRef<[u8]>,
              W: WasmFile<B>,
    {
        let wasm_uri = modules::load(wasm_file)?;
        Self::from_module(
            modules::MODULES.read().unwrap().get(&wasm_uri).as_ref().unwrap(),
            LocalInstanceKey::from(wasm_uri),
        )
    }

    fn from_module(module: &Module, key: LocalInstanceKey) -> anyhow::Result<InstanceEnv> {
        let ins_env = InstanceEnv::from(key);

        let import_object = Self::new_import_object(&module, &ins_env)?;

        let instance = Instance {
            key: ins_env.key.clone(),
            instance: wasmer::Instance::new(&module, &import_object)?,
            loaded: Cell::new(false),
            context: RefCell::new(Context::with_capacity(1024)),
        };
        #[cfg(debug_assertions)]println!("[{:?}] created instance: wasm_uri={}", ins_env.key.thread_id, ins_env.key.wasm_uri);

        ins_env.init_instance(instance);

        // only call once
        if let Err(e) = ins_env.as_instance().init() {
            INSTANCES.write().unwrap().remove(&ins_env.key);
            return Err(e);
        }
        return Ok(ins_env)
    }

    fn new_import_object(module: &Module, ins_env: &InstanceEnv) -> anyhow::Result<ImportObject> {
        let mut import_object = WasiState::new(&ins_env.key.wasm_uri)
            // First, we create the `WasiEnv` with the stdio pipes
            .finalize()?
            // Then, we get the import object related to our WASI
            // and attach it to the Wasm instance.
            .import_object(&module)?;

        import_object.register("env", import_namespace!({
            "_vm_recall" => Function::new_native_with_env(module.store(), ins_env.clone(), |ins_env: &InstanceEnv, is_ctx: i32, offset: i32| {
                let key = &ins_env.key;
                #[cfg(debug_assertions)] println!("[VM:{:?}]_vm_recall: wasm_uri={}, is_ctx={}, offset={}", key.thread_id, key.wasm_uri, is_ctx!=0, offset);
                let ins = ins_env.as_instance();
                ins.ctx_write_to(is_ctx!=0, offset as usize);
            }),
            "_vm_restore" => Function::new_native_with_env(module.store(), ins_env.clone(), |ins_env: &InstanceEnv, offset: i32, size: i32| {
                let key = &ins_env.key;
                #[cfg(debug_assertions)] println!("[VM:{:?}]_vm_restore: wasm_uri={}, offset={}, size={}", key.thread_id, key.wasm_uri, offset, size);
                let ins = ins_env.as_instance();
                let _ = ins.use_ctx_swap_memory(size as usize, |buffer|{
                    ins.read_view_bytes(offset as usize, size as usize, buffer);
                    buffer.len()
                });
            }),
            "_vm_invoke" => Function::new_native_with_env(module.store(), ins_env.clone(), |ins_env: &InstanceEnv, offset: i32, size: i32|-> i32 {
                let key = &ins_env.key;
                #[cfg(debug_assertions)] println!("[VM:{:?}]_vm_invoke: wasm_uri={}, offset={}, size={}", key.thread_id, key.wasm_uri, offset, size);
                let ins = ins_env.as_instance();
                ins.use_ctx_swap_memory(size as usize, |buffer| {
                    ins.read_view_bytes(offset as usize, size as usize, buffer);
                    write_to_vec(&vm_invoke(buffer), buffer)
                }) as i32
            }),
        }));

        Ok(import_object)
    }

    fn init(&self) -> anyhow::Result<()> {
        let ret = self.invoke_instance(WasmHandlerApi::onload_symbol(), None).map_or_else(|e| {
            if e.code == ERR_CODE_NONE.value() {
                #[cfg(debug_assertions)]println!("[{:?}]no need initialize instance: wasm_uri={}", self.key.thread_id, self.key.wasm_uri);
                Ok(())
            } else {
                Err(anyhow!("{}", e))
            }
        }, |_| {
            #[cfg(debug_assertions)]println!("[{:?}]initialized instance: wasm_uri={}", self.key.thread_id, self.key.wasm_uri);
            Ok(())
        });
        self.loaded.set(true);
        ret
    }
    fn check_loaded(&self) -> Result<()> {
        if self.loaded.get() {
            Ok(())
        } else {
            ERR_CODE_NONE.to_result("instance has not completed initialization")
        }
    }
    #[inline]
    pub(crate) fn call_wasm_handler<C: Message>(&self, ctx: Option<C>, method: Method, in_args: InArgs) -> Result<OutRets> {
        self.check_loaded()?;
        #[cfg(debug_assertions)] println!("method={}, data={:?}", in_args.get_method(), in_args.get_data());
        let (ctx_size, args_size) = self.context.borrow_mut().set_args(ctx, in_args);
        let sign_name = WasmHandlerApi::method_to_symbol(method);
        self.invoke_instance(&sign_name, Some((ctx_size as i32, args_size as i32)))?;
        Ok(self.context.borrow_mut().out_rets())
    }
    pub(crate) fn invoke_instance(&self, sign_name: &str, args: Option<(i32, i32)>) -> Result<()> {
        let exports = &self.instance.exports;
        loop {
            let ret = if let Some((ctx_size, args_size)) = args.clone() {
                exports
                    .get_native_function::<(i32, i32), ()>(sign_name)
                    .map_err(|e| ERR_CODE_NONE.to_code_msg(e))?
                    .call(ctx_size, args_size)
            } else {
                exports
                    .get_native_function::<(), ()>(sign_name)
                    .map_err(|e| ERR_CODE_NONE.to_code_msg(e))?
                    .call()
            };
            if let Err(e) = ret {
                let estr = format!("{:?}", e);
                eprintln!("call {} error: {}", sign_name, estr);
                if estr.contains("OOM") {
                    match self.get_memory().grow(1) {
                        Ok(p) => {
                            println!("memory grow, previous memory size: {:?}", p);
                        }
                        Err(e) => {
                            return ERR_CODE_MEM.to_result(format!("failed to memory grow: {:?}", e))
                        }
                    }
                }
            } else {
                return Ok(());
            }
        }
    }
    fn ctx_write_to(&self, is_ctx: bool, offset: usize) {
        let mut ctx = self.context.borrow_mut();
        let cache: &mut Vec<u8> = if is_ctx {
            ctx.ctx_bytes.as_mut()
        } else {
            ctx.swap_memory.as_mut()
        };
        self.set_view_bytes(offset as usize, cache.iter());
        if !is_ctx {
            unsafe { cache.set_len(0); }
        }
    }
    pub(crate) fn use_ctx_swap_memory<F: FnOnce(&mut Vec<u8>) -> usize>(&self, size: usize, call: F) -> usize {
        let mut ctx = self.context.borrow_mut();
        let cache: &mut Vec<u8> = ctx.swap_memory.as_mut();
        if size > 0 {
            resize_with_capacity(cache, size);
        }
        return call(cache);
    }
    fn get_memory(&self) -> &Memory {
        self.instance.exports.get_memory("memory").unwrap()
    }
    fn get_view(&self) -> MemoryView<u8> {
        self.get_memory().view::<u8>()
    }
    fn set_view_bytes<'a>(&self, offset: usize, data: impl IntoIterator<Item=&'a u8> + ExactSizeIterator) {
        let view = self.get_view();
        for (cell, b) in view[offset..offset + data.len()].iter().zip(data) {
            cell.set(*b);
        }
    }
    fn read_view_bytes(&self, offset: usize, size: usize, buffer: &mut Vec<u8>) {
        if size == 0 {
            resize_with_capacity(buffer, size);
            return;
        }
        let view = self.get_view();
        for x in view[offset..(offset + size)]
            .iter()
            .map(|c| c.get()).enumerate() {
            buffer[x.0] = x.1;
        }
    }
}


fn write_to_vec(msg: &dyn Message, buffer: &mut Vec<u8>) -> usize {
    let size = msg.compute_size() as usize;
    resize_with_capacity(buffer, size);
    write_to_with_cached_sizes(msg, buffer)
}

fn write_to_with_cached_sizes(msg: &dyn Message, buffer: &mut Vec<u8>) -> usize {
    let mut os = CodedOutputStream::bytes(buffer);
    msg.write_to_with_cached_sizes(&mut os)
       .or_else(|e| Err(format!("{}", e))).unwrap();
    // os.flush().unwrap();
    buffer.len()
}

fn resize_with_capacity(buffer: &mut Vec<u8>, new_size: usize) {
    if new_size > buffer.capacity() {
        buffer.resize(new_size, 0);
    } else {
        unsafe { buffer.set_len(new_size) };
    }
}
