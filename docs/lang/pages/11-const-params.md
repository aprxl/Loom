title: Const parameters and control flow
summary: `const` parameters, `uint(n)`, `const if`, `const for`, and `target`.
---
This page continues [Compile-time code](page:compile-time). A `const` parameter takes a value that's known while compiling, and the function or warp is specialized for it. `const if` and `const for` decide what code exists.

## `const` parameters

A `const` parameter must be known at compile time. Each distinct value makes its own instance of the function, and the value can appear in types: the length in `[f32; n]` is the parameter. A `const` generic parameter (`const N: usize`) is worked out from the arguments, here from the array's length.

@sample cp_zeros

- The parameter's type is an integer type or `bool` for now. An argument is checked against it ([T0102](diag:T0102)).
- An argument must be something the constant folder can work out: literals, constants, other `const` parameters, arithmetic, comparisons, and the integer methods `log2`, `is_power_of_two`, `min`, `max` and `pow`. Anything else is [K0109](diag:K0109).

@sample cp_err_known

- A `const` function parameter (`fn zeros(const n: usize)`) is still passed at run time like any other. Inside the instance, and in compile-time code, it's its value.
- `zeros(4)` and `zeros(5)` are different instances, and `Ring[u32, 4]` and `Ring[u32, 8]` are different types.

@sample cp_err_range

## Types from numbers

`uint(n)` and `int(n)` are the integer types with `n` bits, for `n` from 1 to 64. `u12` is `uint(12)`. The argument must be known at compile time ([K0109](diag:K0109) if it isn't). Together with `const` parameters, this lets a type follow a number:

@sample cp_types

## Warps with `const` parameters

A generic warp's field types are worked out again for each list of arguments, since `type Idx = uint(N.log2())` can't be resolved in the template. A `const assert` in a warp's body runs for each instance of it, and the error names the instance.

@sample cp_ring

@sample cp_err_assert

## `const if`

`const if cond { ... } else { ... }` is decided while compiling, and the whole `else if` chain with it. The compiler checks, and emits, only the branch it takes. **The others are parsed but never type-checked**, so they can name things that don't exist on this target, such as `std.sys.page_size()` below.

@sample ci_basic

- The condition must be something the constant folder can work out, like a `const` argument: literals, constants, `const` parameters, `target`, arithmetic and comparisons. Otherwise it's [K0110](diag:K0110), or [K0105](diag:K0105) if it reads a local variable.
- It can be an expression. Its branches then have to give a value.

@sample ci_err_known

@sample ci_err_fold

## `const for`

`const for` is unrolled while compiling. Its body is checked once for each item, with the loop variable known at compile time, so each copy can use it in a type (`[0u8; i + 1]` above and below). It goes over a range, an array literal, a constant that holds an array, or `.indexed()` of one of those. It can run at most 10,000 times ([K0102](diag:K0102)). `break` and `continue` can't target a `const for` yet ([U0102](diag:U0102)).

@sample ci_for

## `target`

`target` is a `std.target.Target`, always in scope. It has `os`, `arch`, `endian` and `pointer_bits`, and is known at compile time, so it works in `const if`. On the Lua backend it describes the machine doing the build, and `pointer_bits` is 63. See [std.target](std:target).

@sample ci_target

## Not supported yet

`std.sys`, and `const fn`s that return types are not built. `const` parameters of other types than integers and `bool` aren't either. See [Not supported yet](page:not-yet).
