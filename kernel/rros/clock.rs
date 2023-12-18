#![allow(warnings, unused)]
#![feature(stmt_expr_attributes)]
use crate::{
    factory::{CloneData, RrosElement, RrosFactory, RustFile, RROS_CLONE_PUBLIC},
    factory,
    lock::*,
    tick::*, timer::*, RROS_OOB_CPUS,
    list::*,
    sched::{rros_cpu_rq, this_rros_rq, RQ_TDEFER, RQ_TIMER, RQ_TPROXY},
    thread::T_ROOT,
    tick,
    timeout::RROS_INFINITE,
};

use alloc::rc::Rc;

use core::{
    borrow::{Borrow, BorrowMut},
    cell::{RefCell, UnsafeCell},
    clone::Clone,
    ops::{DerefMut, Deref},
    mem::{align_of, size_of},
    todo,
};

use kernel::{
    bindings, c_types, cpumask, double_linked_list::*, file_operations::{FileOperations, FileOpener}, ktime::*,
    percpu, prelude::*, premmpt, spinlock_init, str::CStr, sync::Lock, sync::SpinLock, sysfs,
    timekeeping,
    clockchips,
    uidgid::{KgidT, KuidT},
    file::File,
    io_buffer::IoBufferWriter,
    device::DeviceType,
};

static mut CLOCKLIST_LOCK: SpinLock<i32> = unsafe { SpinLock::new(1) };

// Define it as a constant here first, and then read it from /dev/rros.
const CONFIG_RROS_LATENCY_USER: KtimeT = 0;
const CONFIG_RROS_LATENCY_KERNEL: KtimeT = 0;
const CONFIG_RROS_LATENCY_IRQ: KtimeT = 0;

// There should be 8.
pub const CONFIG_RROS_NR_CLOCKS: usize = 16;

#[derive(Default)]
pub struct RustFileClock;

impl FileOperations for RustFileClock {
    kernel::declare_file_operations!();
}

pub struct RrosClockGravity {
    irq: KtimeT,
    kernel: KtimeT,
    user: KtimeT,
}

impl RrosClockGravity {
    pub fn new(irq: KtimeT, kernel: KtimeT, user: KtimeT) -> Self {
        RrosClockGravity { irq, kernel, user }
    }
    pub fn get_irq(&self) -> KtimeT {
        self.irq
    }

    pub fn get_kernel(&self) -> KtimeT {
        self.kernel
    }

    pub fn get_user(&self) -> KtimeT {
        self.user
    }

    pub fn set_irq(&mut self, irq: KtimeT) {
        self.irq = irq;
    }

    pub fn set_kernel(&mut self, kernel: KtimeT) {
        self.kernel = kernel;
    }

    pub fn set_user(&mut self, user: KtimeT) {
        self.user = user;
    }
}

pub struct RrosClockOps {
    read: Option<fn(&RrosClock) -> KtimeT>,
    readcycles: Option<fn(&RrosClock) -> u64>,
    set: Option<fn(&mut RrosClock, KtimeT) -> i32>,
    programlocalshot: Option<fn(&RrosClock)>,
    programremoteshot: Option<fn(&RrosClock, *mut RrosRq)>,
    setgravity: Option<fn(&mut RrosClock, RrosClockGravity)>,
    resetgravity: Option<fn(&mut RrosClock)>,
    adjust: Option<fn(&mut RrosClock)>,
}

impl RrosClockOps {
    pub fn new(
        read: Option<fn(&RrosClock) -> KtimeT>,
        readcycles: Option<fn(&RrosClock) -> u64>,
        set: Option<fn(&mut RrosClock, KtimeT) -> i32>,
        programlocalshot: Option<fn(&RrosClock)>,
        programremoteshot: Option<fn(&RrosClock, *mut RrosRq)>,
        setgravity: Option<fn(&mut RrosClock, RrosClockGravity)>,
        resetgravity: Option<fn(&mut RrosClock)>,
        adjust: Option<fn(&mut RrosClock)>,
    ) -> Self {
        RrosClockOps {
            read,
            readcycles,
            set,
            programlocalshot,
            programremoteshot,
            setgravity,
            resetgravity,
            adjust,
        }
    }
}

pub struct RrosClock {
    resolution: KtimeT,
    gravity: RrosClockGravity,
    name: &'static CStr,
    flags: i32,
    ops: RrosClockOps,
    timerdata: *mut RrosTimerbase,
    master: *mut RrosClock,
    offset: KtimeT,
    next: *mut ListHead,
    element: Option<Rc<RefCell<RrosElement>>>,
    dispose: Option<fn(&mut RrosClock)>,
    #[cfg(CONFIG_SMP)]
    pub affinity: Option<cpumask::CpumaskT>,
}

impl RrosClock {
    pub fn new(
        resolution: KtimeT,
        gravity: RrosClockGravity,
        name: &'static CStr,
        flags: i32,
        ops: RrosClockOps,
        timerdata: *mut RrosTimerbase,
        master: *mut RrosClock,
        offset: KtimeT,
        next: *mut ListHead,
        element: Option<Rc<RefCell<RrosElement>>>,
        dispose: Option<fn(&mut RrosClock)>,
        #[cfg(CONFIG_SMP)] affinity: Option<cpumask::CpumaskT>,
    ) -> Self {
        RrosClock {
            resolution,
            gravity,
            name,
            flags,
            ops,
            timerdata,
            master,
            offset,
            next,
            element,
            dispose,
            #[cfg(CONFIG_SMP)]
            affinity,
        }
    }
    pub fn read(&self) -> KtimeT {
        // Error handling.
        if self.ops.read.is_some() {
            return self.ops.read.unwrap()(&self);
        }
        return 0;
    }
    pub fn read_cycles(&self) -> u64 {
        // Error handling.
        if self.ops.readcycles.is_some() {
            return self.ops.readcycles.unwrap()(&self);
        }
        return 0;
    }
    pub fn set(&mut self, time: KtimeT) -> Result<usize> {
        if self.ops.set.is_some() {
            self.ops.set.unwrap()(self, time);
        } else {
            // Prevent the execution of the function if it is null.
            return Err(kernel::Error::EFAULT);
        }
        Ok(0)
    }
    pub fn program_local_shot(&self) {
        if self.ops.programlocalshot.is_some() {
            self.ops.programlocalshot.unwrap()(self);
        }
    }
    pub fn program_remote_shot(&self, rq: *mut RrosRq) {
        if self.ops.programremoteshot.is_some() {
            self.ops.programremoteshot.unwrap()(self, rq);
        }
    }
    pub fn set_gravity(&mut self, gravity: RrosClockGravity) {
        if self.ops.setgravity.is_some() {
            self.ops.setgravity.unwrap()(self, gravity);
        }
    }
    pub fn reset_gravity(&mut self) {
        if self.ops.resetgravity.is_some() {
            self.ops.resetgravity.unwrap()(self);
        }
    }
    pub fn adjust(&mut self) {
        if self.ops.adjust.is_some() {
            self.ops.adjust.unwrap()(self);
        }
    }
    pub fn get_timerdata_addr(&self) -> *mut RrosTimerbase {
        // Error handling.
        return self.timerdata as *mut RrosTimerbase;
    }

    pub fn get_gravity_irq(&self) -> KtimeT {
        self.gravity.get_irq()
    }

    pub fn get_gravity_kernel(&self) -> KtimeT {
        self.gravity.get_kernel()
    }

    pub fn get_gravity_user(&self) -> KtimeT {
        self.gravity.get_user()
    }

    pub fn get_offset(&self) -> KtimeT {
        self.offset
    }

    pub fn get_master(&self) -> *mut RrosClock {
        self.master
    }
}

pub fn adjust_timer(
    clock: &RrosClock,
    timer: Arc<SpinLock<RrosTimer>>,
    tq: &mut List<Arc<SpinLock<RrosTimer>>>,
    delta: KtimeT,
) {
    let date = timer.lock().get_date();
    timer.lock().set_date(ktime_sub(date, delta));
    let is_periodic = timer.lock().is_periodic();
    if is_periodic == false {
        rros_enqueue_timer(timer.clone(), tq);
        return;
    }

    let start_date = timer.lock().get_start_date();
    timer.lock().set_start_date(ktime_sub(start_date, delta));

    let period = timer.lock().get_interval();
    let diff = ktime_sub(clock.read(), rros_get_timer_expiry(timer.clone()));

    if (diff >= period) {
        let div = ktime_divns(diff, ktime_to_ns(period));
        let periodic_ticks = timer.lock().get_periodic_ticks();
        timer
            .lock()
            .set_periodic_ticks((periodic_ticks as i64 + div) as u64);
    } else if (ktime_to_ns(delta) < 0
        && (timer.lock().get_status() & RROS_TIMER_FIRED != 0)
        && ktime_to_ns(ktime_add(diff, period)) <= 0)
    {
        /*
         * Timer is periodic and NOT waiting for its first
         * shot, so we make it tick sooner than its original
         * date in order to avoid the case where by adjusting
         * time to a sooner date, real-time periodic timers do
         * not tick until the original date has passed.
         */
        let div = ktime_divns(-diff, ktime_to_ns(period));
        let periodic_ticks = timer.lock().get_periodic_ticks();
        let pexpect_ticks = timer.lock().get_pexpect_ticks();
        timer
            .lock()
            .set_periodic_ticks((periodic_ticks as i64 - div) as u64);
        timer
            .lock()
            .set_pexpect_ticks((pexpect_ticks as i64 - div) as u64);
    }
    rros_update_timer_date(timer.clone());
    rros_enqueue_timer(timer.clone(), tq);
}

pub fn rros_adjust_timers(clock: &mut RrosClock, delta: KtimeT) {
    // Adjust all timers in the List in each CPU tmb of the current clock.
    // raw_spin_lock_irqsave(&tmb->lock, flags);
    let cpu = 0;
    // for_each_online_cpu(cpu) {
    let rq = rros_cpu_rq(cpu);
    let tmb = rros_percpu_timers(clock, cpu);
    let tq = unsafe { &mut (*tmb).q };

    for i in 1..=tq.len() {
        let timer = tq.get_by_index(i).unwrap().value.clone();
        let get_clock = timer.lock().get_clock();
        if get_clock == clock as *mut RrosClock {
            rros_dequeue_timer(timer.clone(), tq);
            adjust_timer(clock, timer.clone(), tq, delta);
        }
    }

    if rq != this_rros_rq() {
        rros_program_remote_tick(clock, rq);
    } else {
        rros_program_local_tick(clock);
    }
    //}
}

pub fn rros_stop_timers(clock: &RrosClock) {
    let cpu = 0;
    let mut tmb = rros_percpu_timers(&clock, cpu);
    let tq = unsafe { &mut (*tmb).q };
    while tq.is_empty() == false {
        //raw_spin_lock_irqsave(&tmb->lock, flags);
        pr_debug!("rros_stop_timers: 213");
        let timer = tq.get_head().unwrap().value.clone();
        rros_timer_deactivate(timer);
        //raw_spin_unlock_irqrestore(&tmb->lock, flags);
    }
}

// Print the initialization log of the clock.
fn rros_clock_log() {}

fn read_mono_clock(clock: &RrosClock) -> KtimeT {
    timekeeping::ktime_get_mono_fast_ns()
}

fn read_mono_clock_cycles(clock: &RrosClock) -> u64 {
    read_mono_clock(clock) as u64
}

fn set_mono_clock(clock: &mut RrosClock, time: KtimeT) -> i32 {
    // mono cannot be set, the following should be an error type.
    0
}

fn adjust_mono_clock(clock: &mut RrosClock) {}

/**
 * The following functions are the realtime clock operations.
 */

fn read_realtime_clock(clock: &RrosClock) -> KtimeT {
    timekeeping::ktime_get_real_fast_ns()
}

fn read_realtime_clock_cycles(clock: &RrosClock) -> u64 {
    read_realtime_clock(clock) as u64
}

fn set_realtime_clock(clock: &mut RrosClock, time: KtimeT) -> i32 {
    0
}

fn adjust_realtime_clock(clock: &mut RrosClock) {
    // let old_offset: KtimeT = clock.offset;
    // unsafe {
    //     clock.offset = RROS_REALTIME_CLOCK.read() - RROS_MONO_CLOCK.read();
    // }
    // rros_adjust_timers(clock, clock.offset - old_offset)
}

/**
 * The following functions are universal clock operations.
 */

fn get_default_gravity() -> RrosClockGravity {
    RrosClockGravity {
        irq: CONFIG_RROS_LATENCY_IRQ,
        kernel: CONFIG_RROS_LATENCY_KERNEL,
        user: CONFIG_RROS_LATENCY_USER,
    }
}

fn set_coreclk_gravity(clock: &mut RrosClock, gravity: RrosClockGravity) {
    clock.gravity.irq = gravity.irq;
    clock.gravity.kernel = gravity.kernel;
    clock.gravity.user = gravity.user;
}

fn reset_coreclk_gravity(clock: &mut RrosClock) {
    set_coreclk_gravity(clock, get_default_gravity());
}

static RROS_MONO_CLOCK_NAME: &CStr =
    unsafe { CStr::from_bytes_with_nul_unchecked("RROS_CLOCK_MONOTONIC_DEV\0".as_bytes()) };

pub static mut RROS_MONO_CLOCK: RrosClock = RrosClock {
    name: RROS_MONO_CLOCK_NAME,
    resolution: 1,
    gravity: RrosClockGravity {
        irq: CONFIG_RROS_LATENCY_IRQ,
        kernel: CONFIG_RROS_LATENCY_KERNEL,
        user: CONFIG_RROS_LATENCY_USER,
    },
    flags: RROS_CLONE_PUBLIC,
    ops: RrosClockOps {
        read: Some(read_mono_clock),
        readcycles: Some(read_mono_clock_cycles),
        set: None,
        programlocalshot: Some(rros_program_proxy_tick),
        #[cfg(CONFIG_SMP)]
        programremoteshot: Some(rros_send_timer_ipi),
        #[cfg(not(CONFIG_SMP))]
        programremoteshot: None,
        setgravity: Some(set_coreclk_gravity),
        resetgravity: Some(reset_coreclk_gravity),
        adjust: None,
    },
    timerdata: 0 as *mut RrosTimerbase,
    master: 0 as *mut RrosClock,
    next: 0 as *mut ListHead,
    offset: 0,
    element: None,
    dispose: None,
    #[cfg(CONFIG_SMP)]
    affinity: None,
};

static RROS_REALTIME_CLOCK_NAME: &CStr =
    unsafe { CStr::from_bytes_with_nul_unchecked("RROS_CLOCK_REALTIME_DEV\0".as_bytes()) };

pub static mut RROS_REALTIME_CLOCK: RrosClock = RrosClock {
    name: RROS_REALTIME_CLOCK_NAME,
    resolution: 1,
    gravity: RrosClockGravity {
        irq: CONFIG_RROS_LATENCY_IRQ,
        kernel: CONFIG_RROS_LATENCY_KERNEL,
        user: CONFIG_RROS_LATENCY_USER,
    },
    flags: RROS_CLONE_PUBLIC,
    ops: RrosClockOps {
        read: Some(read_realtime_clock),
        readcycles: Some(read_realtime_clock_cycles),
        set: None,
        programlocalshot: None,
        programremoteshot: None,
        setgravity: Some(set_coreclk_gravity),
        resetgravity: Some(reset_coreclk_gravity),
        adjust: Some(adjust_realtime_clock),
    },
    timerdata: 0 as *mut RrosTimerbase,
    master: 0 as *mut RrosClock,
    next: 0 as *mut ListHead,
    offset: 0,
    dispose: None,
    element: None,
    #[cfg(CONFIG_SMP)]
    affinity: None,
};

pub static mut CLOCK_LIST: List<*mut RrosClock> = List::<*mut RrosClock> {
    head: Node::<*mut RrosClock> {
        next: None,
        prev: None,
        value: 0 as *mut RrosClock,
    },
};

pub static mut RROS_CLOCK_FACTORY: SpinLock<factory::RrosFactory> = unsafe {
    SpinLock::new(factory::RrosFactory {
        name: unsafe { CStr::from_bytes_with_nul_unchecked("clock\0".as_bytes()) },
        // fops: Some(&Clockops),
        nrdev: CONFIG_RROS_NR_CLOCKS,
        build: None,
        dispose: Some(clock_factory_dispose),
        attrs: None, //sysfs::attribute_group::new(),
        flags: factory::RrosFactoryType::SINGLE,
        inside: Some(factory::RrosFactoryInside {
            type_: DeviceType::new(),
            class: None,
            cdev: None,
            device: None,
            sub_rdev: None,
            kuid: None,
            kgid: None,
            minor_map: None,
            index: None,
            name_hash: None,
            hash_lock: None,
            register: None,
        }),
    })
};

pub struct ClockOps;

impl FileOpener<u8> for ClockOps {
    fn open(shared: &u8, fileref: &File) -> Result<Self::Wrapper> {
        let mut data = CloneData::default();
        pr_debug!("open clock device success");
        Ok(Box::try_new(data)?)
    }
}

impl FileOperations for ClockOps {
    kernel::declare_file_operations!(read);

    type Wrapper = Box<CloneData>;

    fn read<T: IoBufferWriter>(
        _this: &CloneData,
        _file: &File,
        _data: &mut T,
        _offset: u64,
    ) -> Result<usize> {
        pr_debug!("I'm the read ops of the rros clock factory.");
        Ok(1)
    }
}

pub fn clock_factory_dispose(ele: factory::RrosElement) {}

fn timer_needs_enqueuing(timer: *mut RrosTimer) -> bool {
    unsafe {
        return ((*timer).get_status()
            & (RROS_TIMER_PERIODIC
                | RROS_TIMER_DEQUEUED
                | RROS_TIMER_RUNNING
                | RROS_TIMER_KILLED))
            == (RROS_TIMER_PERIODIC | RROS_TIMER_DEQUEUED | RROS_TIMER_RUNNING);
    }
}

// `rq` related tests haven't been tested, other tests passed.
pub fn do_clock_tick(clock: &mut RrosClock, tmb: *mut RrosTimerbase) {
    let rq = this_rros_rq();
    // #[cfg(CONFIG_RROS_DEBUG_CORE)]
    // if hard_irqs_disabled() == false {
    //     hard_local_irq_disable();
    // }
    let mut tq = unsafe { &mut (*tmb).q };
    //unsafe{(*tmb).lock.lock();}

    unsafe {
        (*rq).add_local_flags(RQ_TIMER);
    }

    let mut now = clock.read();

    // unsafe{
    //     if (*tmb).q.is_empty() == true {
    //         // tick
    //         tick::proxy_set_next_ktime(1000000, 0 as *mut bindings::clock_event_device);
    //     }
    // }

    unsafe {
        while tq.is_empty() == false {
            let mut timer = tq.get_head().unwrap().value.clone();
            let date = (*timer.locked_data().get()).get_date();
            if now < date {
                break;
            }

            rros_dequeue_timer(timer.clone(), tq);

            rros_account_timer_fired(timer.clone());
            (*timer.locked_data().get()).add_status(RROS_TIMER_FIRED);
            let timer_addr = timer.locked_data().get();

            let inband_timer_addr = (*rq).get_inband_timer().locked_data().get();
            if (timer_addr == inband_timer_addr) {
                (*rq).add_local_flags(RQ_TPROXY);
                (*rq).change_local_flags(!RQ_TDEFER);
                continue;
            }
            let handler = (*timer.locked_data().get()).get_handler();
            let c_ref = timer.locked_data().get();
            handler(c_ref);
            now = clock.read();
            let var_timer_needs_enqueuing = timer_needs_enqueuing(timer.locked_data().get());
            if var_timer_needs_enqueuing == true {
                loop {
                    let periodic_ticks = (*timer.locked_data().get()).get_periodic_ticks() + 1;
                    (*timer.locked_data().get()).set_periodic_ticks(periodic_ticks);
                    rros_update_timer_date(timer.clone());

                    let date = (*timer.locked_data().get()).get_date();
                    if date >= now {
                        break;
                    }
                }

                if (rros_timer_on_rq(timer.clone(), rq)) {
                    rros_enqueue_timer(timer.clone(), tq);
                }

                pr_debug!("now is {}", now);
                // pr_debug!("date is {}",timer.lock().get_date());
            }
        }
    }
    unsafe { (*rq).change_local_flags(!RQ_TIMER) };

    rros_program_local_tick(clock as *mut RrosClock);

    //raw_spin_unlock(&tmb->lock);
}

pub struct RrosCoreTick;

impl clockchips::CoreTick for RrosCoreTick {
    fn core_tick(dummy: clockchips::ClockEventDevice) {
        // pr_debug!("in rros_core_tick");
        let this_rq = this_rros_rq();
        //	if (RROS_WARN_ON_ONCE(CORE, !is_rros_cpu(rros_rq_cpu(this_rq))))
        // pr_info!("in rros_core_tick");
        unsafe {
            do_clock_tick(&mut RROS_MONO_CLOCK, rros_this_cpu_timers(&RROS_MONO_CLOCK));

            let rq_has_tproxy = ((*this_rq).local_flags & RQ_TPROXY != 0x0);
            let assd = (*(*this_rq).get_curr().locked_data().get()).state;
            let curr_state_is_t_root = (assd & (T_ROOT as u32) != 0x0);
            // This `if` won't enter, so there is a problem.
            // let a = ((*this_rq).local_flags & RQ_TPROXY != 0x0);
            // if rq_has_tproxy  {
            //     pr_debug!("in rros_core_tick");
            //     pr_debug!("in rros_core_tick");
            //     pr_debug!("in rros_core_tick");
            //     pr_debug!("in rros_core_tick");
            //     pr_debug!("in rros_core_tick");
            //     pr_debug!("in rros_core_tick");
            //     pr_debug!("in rros_core_tick");
            //     pr_debug!("in rros_core_tick");
            // }
            // let b = ((*this_rq).get_curr().lock().deref_mut().state & (T_ROOT as u32) != 0x0);

            // if curr_state_is_t_root  {
            //     pr_debug!("in rros_core_tick");
            //     pr_debug!("in rros_core_tick");
            //     pr_debug!("in rros_core_tick");
            //     pr_debug!("in rros_core_tick");
            //     pr_debug!("in rros_core_tick");
            //     pr_debug!("in rros_core_tick");
            //     pr_debug!("in rros_core_tick");
            //     pr_debug!("in rros_core_tick");
            // }
            if rq_has_tproxy && curr_state_is_t_root {
                rros_notify_proxy_tick(this_rq);
            }
        }
    }
}

fn init_clock(clock: *mut RrosClock, master: *mut RrosClock) -> Result<usize> {
    // unsafe{
    //     if (*clock).element.is_none(){
    //         return Err(kernel::Error::EINVAL);
    //     }
    // }
    // unsafe{
    //     factory::rros_init_element((*clock).element.as_ref().unwrap().clone(),
    //     &mut RROS_CLOCK_FACTORY, (*clock).flags & RROS_CLONE_PUBLIC);
    // }
    unsafe {
        (*clock).master = master;
    }
    //rros_create_core_element_device()?;

    unsafe {
        CLOCKLIST_LOCK.lock();
        CLOCK_LIST.add_head(clock);
        CLOCKLIST_LOCK.unlock();
    }

    Ok(0)
}

fn rros_init_slave_clock(clock: &mut RrosClock, master: &mut RrosClock) -> Result<usize> {
    premmpt::running_inband()?;

    // TODO: Check if there is a problem here, even if the timer can run.
    // #[cfg(CONFIG_SMP)]
    // clock.affinity = master.affinity;

    clock.timerdata = master.get_timerdata_addr();
    clock.offset = clock.read() - master.read();
    init_clock(clock as *mut RrosClock, master as *mut RrosClock)?;
    Ok(0)
}

fn rros_init_clock(clock: &mut RrosClock, affinity: &cpumask::CpumaskT) -> Result<usize> {
    premmpt::running_inband()?;
    // 8 byte alignment
    let tmb = percpu::alloc_per_cpu(
        size_of::<RrosTimerbase>() as usize,
        align_of::<RrosTimerbase>() as usize,
    ) as *mut RrosTimerbase;
    if tmb == 0 as *mut RrosTimerbase {
        return Err(kernel::Error::ENOMEM);
    }
    clock.timerdata = tmb;

    let mut tmb = rros_percpu_timers(clock, 0);

    unsafe {
        raw_spin_lock_init(&mut (*tmb).lock);
    }

    clock.offset = 0;
    let ret = init_clock(clock as *mut RrosClock, clock as *mut RrosClock);
    if let Err(_) = ret {
        percpu::free_per_cpu(clock.get_timerdata_addr() as *mut u8);
        return ret;
    }
    Ok(0)
}

pub fn rros_clock_init() -> Result<usize> {
    let pinned = unsafe { Pin::new_unchecked(&mut CLOCKLIST_LOCK) };
    spinlock_init!(pinned, "CLOCKLIST_LOCK");
    unsafe {
        RROS_MONO_CLOCK.reset_gravity();
        RROS_REALTIME_CLOCK.reset_gravity();
        rros_init_clock(&mut RROS_MONO_CLOCK, &RROS_OOB_CPUS)?;
    }
    let ret = unsafe { rros_init_slave_clock(&mut RROS_REALTIME_CLOCK, &mut RROS_MONO_CLOCK) };
    if let Err(_) = ret {
        //rros_put_element(&rros_mono_clock.element);
    }
    pr_debug!("clock init success!");
    Ok(0)
}

pub fn rros_read_clock(clock: &RrosClock) -> KtimeT {
    let clock_add = clock as *const RrosClock;
    let mono_add = unsafe { &RROS_MONO_CLOCK as *const RrosClock };

    if (clock_add == mono_add) {
        return rros_ktime_monotonic();
    }

    clock.ops.read.unwrap()(&clock)
}

fn rros_ktime_monotonic() -> KtimeT {
    timekeeping::ktime_get_mono_fast_ns()
}
