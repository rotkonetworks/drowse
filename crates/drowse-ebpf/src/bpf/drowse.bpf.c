// SPDX-License-Identifier: GPL-2.0
//
// drowse wakeup attribution (M2). See design doc §6.
//
// A per-CPU context stack is pushed/popped by timer/irq/softirq/workqueue raw
// tracepoints. At sched:sched_waking (waker context — the brief's correctness
// point: we blame whoever is running NOW on the waker CPU, not the woken task),
// one count lands in an LRU per-CPU hash keyed {kind, id}. Each count is tagged
// with whether the target CPU was idle (idle_break) and whether the whole
// package was asleep (pkg_break). Nothing crosses to userspace per event; the
// loader drains the maps in batch on a slow cadence.

#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include <bpf/bpf_core_read.h>

char LICENSE[] SEC("license") = "GPL";

// Mirror of drowse_api::OffenderKind, by discriminant order.
#define KIND_PROCESS    0
#define KIND_TIMER      1
#define KIND_IRQ        2
#define KIND_SOFTIRQ    3
#define KIND_WORKQUEUE  4

#define MAX_STACK   8
#define STACK_MASK  (MAX_STACK - 1)
#define MAX_CPUS    256
// power:cpu_idle uses PWR_EVENT_EXIT (-1) as the "leaving idle" state.
#define PWR_EVENT_EXIT 0xffffffffU

// Keyed {kind, id}: pid, callback address, irq line, or softirq vector.
struct okey {
    __u64 id;
    __u32 kind;
    __u32 _pad;
};

struct oval {
    __u64 total;
    __u64 idle_breaks;
    __u64 pkg_breaks;
    char  comm[16]; // filled in-kernel for Process; else resolved userspace
};

struct ctx_entry {
    __u64 id;
    __u32 kind;
    __u32 _pad;
};

struct ctx_stack {
    __u32 depth;
    struct ctx_entry e[MAX_STACK];
};

// Set by the loader before load() via the skeleton's rodata.
const volatile __u32 nr_cpus = 1;
const volatile __u32 self_tgid = 0;

struct {
    __uint(type, BPF_MAP_TYPE_LRU_PERCPU_HASH);
    __uint(max_entries, 16384);
    __type(key, struct okey);
    __type(value, struct oval);
} offenders SEC(".maps");

// Per-CPU interrupt/timer/work context stack.
struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct ctx_stack);
} ctxstack SEC(".maps");

// Idle flag per CPU. A plain ARRAY (not per-cpu) so sched_waking can read the
// idle state of the *target* CPU, which is not the CPU it runs on.
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, MAX_CPUS);
    __type(key, __u32);
    __type(value, __u32);
} idle_flag SEC(".maps");

// [0] = number of CPUs currently idle (global, atomically maintained).
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, __u64);
} idle_count SEC(".maps");

// [0] = the daemon's own wakeups (self-exclusion bucket), per-cpu.
struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, __u64);
} observer SEC(".maps");

static __always_inline void push_ctx(__u32 kind, __u64 id)
{
    __u32 z = 0;
    struct ctx_stack *s = bpf_map_lookup_elem(&ctxstack, &z);
    if (!s)
        return;
    __u32 d = s->depth;
    if (d < MAX_STACK) {
        __u32 i = d & STACK_MASK;
        s->e[i].kind = kind;
        s->e[i].id = id;
    }
    // Always advance depth so entry/exit stay balanced even past MAX_STACK.
    s->depth = d + 1;
}

static __always_inline void pop_ctx(void)
{
    __u32 z = 0;
    struct ctx_stack *s = bpf_map_lookup_elem(&ctxstack, &z);
    if (!s)
        return;
    if (s->depth > 0)
        s->depth -= 1;
}

SEC("tp_btf/hrtimer_expire_entry")
int BPF_PROG(on_hrtimer_entry, struct hrtimer *timer, ktime_t now)
{
    push_ctx(KIND_TIMER, (__u64)BPF_CORE_READ(timer, function));
    return 0;
}

SEC("tp_btf/hrtimer_expire_exit")
int BPF_PROG(on_hrtimer_exit, struct hrtimer *timer)
{
    pop_ctx();
    return 0;
}

SEC("tp_btf/timer_expire_entry")
int BPF_PROG(on_timer_entry, struct timer_list *timer, unsigned long baseclk)
{
    push_ctx(KIND_TIMER, (__u64)BPF_CORE_READ(timer, function));
    return 0;
}

SEC("tp_btf/timer_expire_exit")
int BPF_PROG(on_timer_exit, struct timer_list *timer)
{
    pop_ctx();
    return 0;
}

SEC("tp_btf/irq_handler_entry")
int BPF_PROG(on_irq_entry, int irq, struct irqaction *action)
{
    push_ctx(KIND_IRQ, (__u64)(__u32)irq);
    return 0;
}

SEC("tp_btf/irq_handler_exit")
int BPF_PROG(on_irq_exit, int irq, struct irqaction *action, int ret)
{
    pop_ctx();
    return 0;
}

SEC("tp_btf/softirq_entry")
int BPF_PROG(on_softirq_entry, unsigned int vec_nr)
{
    push_ctx(KIND_SOFTIRQ, (__u64)vec_nr);
    return 0;
}

SEC("tp_btf/softirq_exit")
int BPF_PROG(on_softirq_exit, unsigned int vec_nr)
{
    pop_ctx();
    return 0;
}

SEC("tp_btf/workqueue_execute_start")
int BPF_PROG(on_wq_start, struct work_struct *work)
{
    push_ctx(KIND_WORKQUEUE, (__u64)BPF_CORE_READ(work, func));
    return 0;
}

SEC("tp_btf/workqueue_execute_end")
int BPF_PROG(on_wq_end, struct work_struct *work, work_func_t function)
{
    pop_ctx();
    return 0;
}

SEC("tp_btf/cpu_idle")
int BPF_PROG(on_cpu_idle, unsigned int state, unsigned int cpu_id)
{
    __u32 z = 0;
    if (cpu_id >= MAX_CPUS)
        return 0;
    __u32 *f = bpf_map_lookup_elem(&idle_flag, &cpu_id);
    if (!f)
        return 0;

    if (state == PWR_EVENT_EXIT) {
        if (*f) {
            *f = 0;
            __u64 *ic = bpf_map_lookup_elem(&idle_count, &z);
            if (ic && *ic > 0)
                __sync_fetch_and_add(ic, -(__u64)1);
        }
    } else {
        if (!*f) {
            *f = 1;
            __u64 *ic = bpf_map_lookup_elem(&idle_count, &z);
            if (ic)
                __sync_fetch_and_add(ic, 1);
        }
    }
    return 0;
}

SEC("tp_btf/sched_waking")
int BPF_PROG(on_waking, struct task_struct *p)
{
    __u32 z = 0;
    __u32 kind;
    __u64 id;

    // Blame the current (waker) context: the top of this CPU's interrupt/timer
    // stack, or the running task itself when the stack is empty.
    struct ctx_stack *s = bpf_map_lookup_elem(&ctxstack, &z);
    if (s && s->depth > 0) {
        __u32 d = (s->depth - 1) & STACK_MASK;
        kind = s->e[d].kind;
        id = s->e[d].id;
    } else {
        kind = KIND_PROCESS;
        id = bpf_get_current_pid_tgid() >> 32;
    }

    // Self-exclusion: the daemon's own wakeups go to a separate bucket.
    if (kind == KIND_PROCESS && (__u32)id == self_tgid) {
        __u64 *o = bpf_map_lookup_elem(&observer, &z);
        if (o)
            *o += 1;
        return 0;
    }

    // Idle gating: inspect the CPU the woken task will land on.
    __u32 tcpu = (__u32)BPF_CORE_READ(p, wake_cpu);
    bool idle_break = false, pkg_break = false;
    if (tcpu < MAX_CPUS) {
        __u32 *f = bpf_map_lookup_elem(&idle_flag, &tcpu);
        if (f && *f) {
            idle_break = true;
            __u64 *ic = bpf_map_lookup_elem(&idle_count, &z);
            // Package break: this is the only non-idle CPU (the waker), so the
            // whole package was asleep and this wakeup broke it.
            if (ic && *ic + 1 >= nr_cpus)
                pkg_break = true;
        }
    }

    struct okey k = {};
    k.id = id;
    k.kind = kind;

    struct oval *v = bpf_map_lookup_elem(&offenders, &k);
    if (v) {
        v->total += 1;
        if (idle_break)
            v->idle_breaks += 1;
        if (pkg_break)
            v->pkg_breaks += 1;
        if (kind == KIND_PROCESS && v->comm[0] == 0)
            bpf_get_current_comm(v->comm, sizeof(v->comm));
    } else {
        struct oval nv = {};
        nv.total = 1;
        nv.idle_breaks = idle_break ? 1 : 0;
        nv.pkg_breaks = pkg_break ? 1 : 0;
        if (kind == KIND_PROCESS)
            bpf_get_current_comm(nv.comm, sizeof(nv.comm));
        bpf_map_update_elem(&offenders, &k, &nv, BPF_ANY);
    }
    return 0;
}
