use wasmtime::{Engine, Instance, Module, Store};

fn main() {
    let engine = Engine::default();
    let wat = r#"(module
        (func (export "add") (param i32 i32) (result i32) local.get 0 local.get 1 i32.add)
        (func (export "boom") unreachable))"#;
    let module = Module::new(&engine, wat).expect("compile");
    let mut store = Store::new(&engine, ());
    let inst = Instance::new(&mut store, &module, &[]).expect("instantiate");
    let add = inst.get_typed_func::<(i32, i32), i32>(&mut store, "add").unwrap();
    let boom = inst.get_typed_func::<(), ()>(&mut store, "boom").unwrap();
    let sum = add.call(&mut store, (2, 40)).unwrap();
    let trap = boom.call(&mut store, ()).unwrap_err();
    println!("{{\"add\":{sum},\"trap\":\"{:?}\"}}", trap.downcast_ref::<wasmtime::Trap>());
}
