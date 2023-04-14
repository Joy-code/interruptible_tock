// Inspired by alarm_edge_cases.rs

use core::cell::Cell;
use kernel::debug;
use kernel::hil::time::Alarm;
use kernel::static_init;
use sam4l::ast::Ast;

/// Stores a reference to a Virtual Timer
#[no_mangle]
#[used]
pub static mut VIRTUAL_TIMER: BenchmarkingTimer<'static, Ast<'static>>;

pub struct BenchmarkingTimer<'a, A: 'a> {
    alarm: &'a A,
    counter: Cell<usize>,
    start: u32,
}

impl<'a, A: Alarm<'a>> BenchmarkingTimer<'a, A> {
    pub fn new(alarm: &'a A) -> BenchmarkingTimer<'a, A> {
        BenchmarkingTimer {
            alarm: alarm,
            counter: Cell::new(0),
            start: 0,
        }
    }

    pub fn increment_counter(&self) {
        let counter = self.counter.get();
        if counter == 0 {
            let delay = self.alarm.ticks_from_ms(10000);
            let now = self.alarm.now();
            let start = now.wrapping_sub(A::Ticks::from(10));

            debug!(
                "{}: Setting alarm to {} + {} = {}",
                now.into_u32(),
                start.into_u32(),
                delay.into_u32(),
                start.wrapping_add(delay).into_u32()
            );

            self.alarm.set_alarm(start, delay);
            self.start = start.into_u32()
        } else if counter == 100 {
            let alarm = self.alarm.get_alarm();
            let now = self.alarm.now();

            debug!(
                "{}: Benchmark lasted for {} - {} = {}",
                now.into_u32(),
                now.into_u32(),
                start,
                now.into_u32() - start,
            );
            panic!();
        }

        self.counter.set(counter + 1);
    }
}

impl<'a, A: Alarm<'a>> AlarmClient for BenchmarkingTimer<'a, A> {
    fn alarm(&self) {
        let now = self.alarm.now();
        debug!("Alarm fired at {}.", now.into_u32());
    }
}

unsafe fn static_init_benchmarking(
    ast: &'static Ast,
) -> &'static BenchmarkingTimer<'static, Ast<'static>> {
    let benchmark = static_init!(
        BenchmarkingTimer<'static, Ast<'static>>,
        BenchmarkingTimer::new(ast)
    );
    ast.set_alarm_client(benchmark);
    VIRTUAL_TIMER = benchmark;
    benchmark
}

unsafe fn update_benchmark() {
    (VIRTUAL_TIMER as BenchmarkingTimer<'static, Ast<'static>>).increment_counter();
}
