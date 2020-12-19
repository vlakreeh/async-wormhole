#![feature(asm, naked_functions)]

//! Switcheroo provides lightweight context switches in Rust.
//!
//! It consists of two parts:
//! 1. A stack implementation (currently only providing a [fixed 8Mb stack](stack/struct.EightMbStack.html)).
//! 2. A [generator](struct.Generator.html) implementation.
//! ## Example
//! ```
//! use switcheroo::stack::*;
//! use switcheroo::Generator;
//!
//! fn  main() {
//! 	let stack = EightMbStack::new().unwrap();
//! 	let  mut add_one = Generator::new(stack, |yielder, mut input| {
//! 		loop {
//! 			if input ==  0 {
//! 				break;
//! 			}
//! 			input = yielder.suspend(input +  1);
//! 		}
//! 	});
//!
//! 	assert_eq!(add_one.resume(2), Some(3));
//! 	assert_eq!(add_one.resume(127), Some(128));
//! 	assert_eq!(add_one.resume(0), None);
//! 	assert_eq!(add_one.resume(0), None);
//! }
// ```

mod arch;
pub mod stack;

use std::{any::Any, rc::Rc};
use std::cell::Cell;
use std::marker::PhantomData;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::{mem, ptr::NonNull};

// Communicates the return of the Generator.
enum GeneratorOutput<Output> {
    // The generator returned a regular value.
    Value(Output),
    // The generator finished and there are no more values to be returned.
    Finished,
    // The generator panicked. This value is passed to `resume_unwind` to continue the unwind
    // across contexts.
    Panic(Box<dyn Any + Send + 'static>), // Err part of std::thread::Result
}

/// Generator wraps a closure and allows suspending its execution more than once, returning
/// a value each time.
///
/// If the closure finishes each other call to [resume](struct.Generator.html#method.resume)
/// will yield `None`. If the closure panics the unwind will happen correctly across contexts.
pub struct Generator<'a, Input: 'a, Output: 'a, Stack: stack::Stack> {
    stack: Stack,
    stack_ptr: Option<NonNull<usize>>,
    phantom: PhantomData<(&'a (), *mut Input, *const Output)>,
}

impl<'a, Input, Output, Stack> Generator<'a, Input, Output, Stack>
where
    Input: 'a,
    Output: 'a,
    Stack: stack::Stack,
{
    /// Create a new generator from a stack and closure.
    pub fn new<F>(stack: Stack, f: F) -> Generator<'a, Input, Output, Stack>
    where
        F: FnOnce(Rc<Yielder<Input, Output>>, Input) + 'a,
    {
        // This function will be written to the new stack (by `arch::init`) as the initial
        // entry point. During the `arch::swap_and_link_stacks` call it will be called with
        // the correct closure passed as the first argument. This function will never return.
        // Yielding back into it after `yielder.suspend_(GeneratorOutput::Finished)` was
        // called would be undefined behavior.
        unsafe extern "C" fn generator_wrapper<Input, Output, Stack, F>(
            f_ptr: usize,
            stack_ptr: *mut usize,
        ) where
            Stack: stack::Stack,
            F: FnOnce(Rc<Yielder<Input, Output>>, Input),
        {
            let f = std::ptr::read(f_ptr as *const F);
            let (data, stack_ptr) = arch::swap(0, stack_ptr);
            let input = std::ptr::read(data as *const Input);
            let yielder = Yielder::new(stack_ptr);
            let yielder = Rc::new(yielder);

            // It is not safe to unwind across the context switch directly.
            // The unwind will continue in the original context.
            match catch_unwind(AssertUnwindSafe(|| {
                f(yielder.clone(), input);
            })) {
                Ok(_) => yielder.suspend_(GeneratorOutput::Finished),
                Err(panic) => yielder.suspend_(GeneratorOutput::Panic(panic)),
            };
        }

        // Prepare the stack
        let stack_ptr = unsafe { arch::init(&stack, generator_wrapper::<Input, Output, Stack, F>) };

        // f needs to live on after this function, it is part of the new context. This prevents it
        // from being dropped.
        let f = mem::ManuallyDrop::new(f);

        // This call will link the stacks together with assembly directives magic, but once the
        // first `arch::swap` inside `generator_wrapper` is reached it will yield back before the
        // execution of the closure `f`.
        // Only the next call to `resume` will start executing the closure.
        let stack_ptr = unsafe {
            arch::swap_and_link_stacks(
                &f as *const mem::ManuallyDrop<F> as usize,
                stack_ptr,
                stack.bottom(),
            )
            .1
        };

        Generator {
            stack,
            stack_ptr: Some(NonNull::new(stack_ptr).unwrap()),
            phantom: PhantomData,
        }
    }

    /// Resume the generator yielding the next value.
    #[inline(always)]
    pub fn resume(&mut self, input: Input) -> Option<Output> {
        if self.stack_ptr.is_none() {
            return None;
        };
        let stack_ptr = self.stack_ptr.unwrap();

        unsafe {
            let input = mem::ManuallyDrop::new(input);
            let (data_out, stack_ptr) = arch::swap(
                &input as *const mem::ManuallyDrop<Input> as usize,
                stack_ptr.as_ptr(),
            );

            let output = std::ptr::read(data_out as *const GeneratorOutput<Output>);
            match output {
                GeneratorOutput::Value(value) => {
                    self.stack_ptr = Some(NonNull::new(stack_ptr).unwrap());
                    Some(value)
                }
                GeneratorOutput::Finished => {
                    self.stack_ptr = None;
                    None
                }
                GeneratorOutput::Panic(panic) => {
                    resume_unwind(panic);
                }
            }
        }
    }

    /// Consume the generator and extract the stack.
    pub fn stack(self) -> Stack {
        self.stack
    }
}

/// Yielder is an interface provided to every generator through which it returns a value.
pub struct Yielder<Input, Output> {
    stack_ptr: Cell<*mut usize>,
    phantom: PhantomData<(*const Input, *mut Output)>,
}

impl<Input, Output> Yielder<Input, Output> {
    fn new(stack_ptr: *mut usize) -> Yielder<Input, Output> {
        Yielder {
            stack_ptr: Cell::new(stack_ptr),
            phantom: PhantomData,
        }
    }

    /// Suspends the generator and returns `Some(val)` from the `resume()` invocation that resumed
    /// the generator.
    #[inline(always)]
    pub fn suspend(&self, val: Output) -> Input {
        unsafe { self.suspend_(GeneratorOutput::Value(val)) }
    }

    #[inline(always)]
    unsafe fn suspend_(&self, out: GeneratorOutput<Output>) -> Input {
        let out = mem::ManuallyDrop::new(out);
        let (data, stack_ptr) = arch::swap(
            &out as *const mem::ManuallyDrop<GeneratorOutput<Output>> as usize,
            self.stack_ptr.get(),
        );
        self.stack_ptr.set(stack_ptr);
        std::ptr::read(data as *const Input)
    }
}
