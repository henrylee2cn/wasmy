#![feature(thread_id_value)]

use std::thread;

use rand::random;

use wasmy_abi::*;
use wasmy_abi::test::*;

static mut STATE: u64 = 0;

/// initialization
#[wasm_onload]
fn init() {
    unsafe {
        STATE = thread::current().id().as_u64().get();
        println!("[Wasm-Simple] initialized STATE to thread id: {}", STATE);
    }
}

#[wasm_handle(0)]
fn multiply(ctx: Ctx, args: TestArgs) -> Result<TestRets> {
    let rid = random::<u8>() as i32;
    unsafe {
        STATE += 1;
        println!("[Wasm-Simple({})] STATE={}, handle guest method({}) ctx={:?}, args={{{:?}}}", rid, STATE, 0, ctx, args);
    }
    let mut host_args = TestArgs::new();
    host_args.a = rid;
    host_args.b = rid;
    let host_res: TestRets = ctx.call_host(0, &host_args)?;
    println!("[Wasm-Simple({})] call host method({}): args={{{:?}}}, result={}", rid, 0, host_res, host_res.get_c());

    let mut res = TestRets::new();
    res.set_c(args.a * args.b);
    Ok(res)
}


// Expanded codes:
//
// #[allow(redundant_semicolons)]
// #[inline]
// #[no_mangle]
// pub extern "C" fn
// _wasm_onload()
// {
//     /// initialization
//     fn init()
//     {
//         unsafe
//             {
//                 STATE = thread::current().id().as_u64().get();
//                 println!("[Wasm-Simple] initialized STATE to thread id: {}", STATE);
//             }
//     }
//     ;
//     init();
// }
//
// fn multiply(ctx: Ctx, args: TestArgs) -> Result<TestRets>
// {
//     let rid = random::<u8>() as i32;
//     unsafe
//         {
//             STATE += 1;
//             println!("[Wasm-Simple({})] STATE={}, handle guest method({}) ctx={:?}, args={{{:?}}}",
//                      rid, STATE, 0, ctx, args);
//         }
//     let mut host_args = TestArgs::new();
//     host_args.a = rid;
//     host_args.b
//         = rid;
//     let host_res: TestRets = ctx.call_host(0, &host_args)?;
//     println!("[Wasm-Simple({})] call host method({}): args={{{:?}}}, result={}", rid,
//              0, host_res, host_res.get_c());
//     let mut res = TestRets::new();
//     res.set_c(args.a * args.b);
//     Ok(res)
// }
//
// #[allow(redundant_semicolons)]
// #[inline]
// #[no_mangle]
// pub extern "C" fn
// _wasm_handle_0(ctx_id: i32, size: i32)
// {
//     #[inline]
//     fn
//     _inner(ctx: ::wasmy_abi::Ctx, args: ::wasmy_abi::InArgs) -> ::
//     wasmy_abi::Result<::wasmy_abi::Any>
//     { ::wasmy_abi::pack_any(multiply(ctx, args.get_args()?)?) }
//     ;
//     ::
//     wasmy_abi::wasm_handle(ctx_id, size, _inner)
// }
