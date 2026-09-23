//! The one owner of the bundled Lua VM.
//!
//! Everything that executes Lua goes through this crate: `loom run` (via the
//! hidden `lua-host` subcommand), the test harness, and compile-time `const`
//! evaluation. Exactly one of the `lua54` / `lua55` features is enabled.
//!
//! Only the VM bootstrap exists so far. Planned modules (see the plan in
//! `design/lua-backend.md`): `runtime` (the embedded `rt` chunk), `hostio`,
//! `hooks` (instruction budget, Ctrl-C), `luamap` (source maps), `remap`
//! (stderr rewriting), `exit` (exit-code mapping).

use mlua::{Lua, Table};

#[cfg(all(feature = "lua54", feature = "lua55"))]
compile_error!("enable exactly one of the `lua54` and `lua55` features");

#[cfg(not(any(feature = "lua54", feature = "lua55")))]
compile_error!("enable one of the `lua54` and `lua55` features");

/// The Lua version this build embeds, as reported by `_VERSION`.
#[cfg(feature = "lua54")]
pub const LUA_VERSION: &str = "Lua 5.4";
/// The Lua version this build embeds, as reported by `_VERSION`.
#[cfg(feature = "lua55")]
pub const LUA_VERSION: &str = "Lua 5.5";

/// Source of the runtime core chunk, embedded at build time.
pub const RT_CORE: &str = include_str!("../../../runtime/rt/core.lua");

/// A fresh Lua state with the Loom runtime loaded.
#[derive(Debug)]
pub struct Vm {
    lua: Lua,
}

impl Vm {
    /// Creates a VM with the standard libraries and the `rt` table loaded.
    pub fn new() -> mlua::Result<Self> {
        let lua = Lua::new();
        let rt: Table = lua.load(RT_CORE).set_name("@loomrt/core.lua").eval()?;
        lua.globals().set("__loomrt", rt)?;
        Ok(Self { lua })
    }

    /// The underlying Lua state.
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// The `rt` table of this VM.
    pub fn rt(&self) -> mlua::Result<Table> {
        self.lua.globals().get("__loomrt")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_the_expected_lua_version() -> mlua::Result<()> {
        let vm = Vm::new()?;
        let version: String = vm.lua().globals().get("_VERSION")?;
        assert_eq!(version, LUA_VERSION);
        Ok(())
    }

    #[test]
    fn loads_the_runtime_core() -> mlua::Result<()> {
        let vm = Vm::new()?;
        let v: String = vm.rt()?.get("VERSION")?;
        assert_eq!(v, "0.1.0");
        Ok(())
    }

    #[test]
    fn has_native_integers_and_bitwise_ops() -> mlua::Result<()> {
        // The backend relies on 64-bit integers, `//`, and native bitwise operators.
        let vm = Vm::new()?;
        let (kind, wrapped, shifted): (String, i64, i64) = vm
            .lua()
            .load("return math.type(7 // 2), math.maxinteger + 1, (1 << 63) >> 63")
            .eval()?;
        assert_eq!(kind, "integer");
        assert_eq!(wrapped, i64::MIN);
        assert_eq!(shifted, 1);
        Ok(())
    }
}
