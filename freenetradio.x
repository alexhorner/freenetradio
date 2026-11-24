/* FreeNetRadio's linker script: qingke-rt 0.8's link.x with a .slow section
 * added before .text, for cold code and data in the slow (non-zero-wait)
 * part of flash. A separate INSERT script doesn't work here: lld gives the
 * main script's `*(.text .text.*)` first pick, so nothing would move.
 *
 * .slow keeps the 192K zero-wait region for what needs speed: the MP3
 * decoder, audio, display, the TCP/IP core and interrupt handlers. Rust puts
 * each function in its own .text.<v0-mangled-name> section (function-sections
 * is enabled in the target JSON) and v0 names include module paths and task
 * names, so functions can be picked by name. Moved: anything marked
 * #[link_section = ".slow..."] (the web page), the web server task, the
 * stream client task (network-paced, 16 KB/s), startup code in main, the
 * remote-button handler, smoltcp's DHCP and DNS clients, and TLS. NOT moved:
 * presets.rs, whose flash writes must not run from the flash being written.
 */
INCLUDE memory.x
/* Provides weak aliases (cf. PROVIDED) for device specific interrupt handlers */
/* This will usually be provided by a device crate generated using svd2rust (see `device.x`) */
INCLUDE device.x

PROVIDE(_stext = ORIGIN(REGION_TEXT));
PROVIDE(_stack_start = ORIGIN(REGION_STACK) + LENGTH(REGION_STACK));
PROVIDE(_max_hart_id = 0);
PROVIDE(_hart_stack_size = 2K);
PROVIDE(_heap_size = 0);

/* fault handlers */
PROVIDE(InstructionMisaligned = ExceptionHandler);
PROVIDE(InstructionFault = ExceptionHandler);
PROVIDE(IllegalInstruction = ExceptionHandler);
PROVIDE(Breakpoint = ExceptionHandler);
PROVIDE(LoadMisaligned = ExceptionHandler);
PROVIDE(LoadFault = ExceptionHandler);
PROVIDE(StoreMisaligned = ExceptionHandler);
PROVIDE(StoreFault = ExceptionHandler);;
PROVIDE(UserEnvCall = ExceptionHandler);
PROVIDE(MachineEnvCall = ExceptionHandler);

/* core interrupt handlers */
PROVIDE(NonMaskableInt = DefaultHandler);
PROVIDE(SysTick = DefaultHandler);
PROVIDE(Software = DefaultHandler);

PROVIDE(Exception = _exception_handler);

PROVIDE(DefaultHandler = DefaultInterruptHandler);
PROVIDE(ExceptionHandler = DefaultExceptionHandler);

/* # Interrupt vectors */
EXTERN(__CORE_INTERRUPTS);
EXTERN(__EXTERNAL_INTERRUPTS); /* `static` variable similar to `__EXCEPTIONS` */

ENTRY(_start)

SECTIONS
{
    .vector_table ORIGIN(FLASH) :
    {
        KEEP(*(SORT_NONE(.init)))
        . = ALIGN(4);
        /* core interrupts table's first entry is omitted, occupied by the init jump instruction */
        KEEP(*(.vector_table.core_interrupts));
        KEEP(*(.vector_table.external_interrupts));
        KEEP(*(.vector_table.exceptions));
        *(.trap .trap.rust)
    } >FLASH AT>FLASH

    /* The inner loop of P-256 scalar multiplication (field add/sub/mul,
     * point add/double), ~6 KB, in zero-wait flash. The TLS key exchange
     * runs two scalar multiplications with no await in between: from slow
     * flash each blocked the executor for ~600 ms, long enough for the
     * Ethernet RX ring to overflow and https start-ups to stall or fail.
     * Listed before .slow so these win over its p256 / embedded_tls
     * patterns (the point functions' names end in embedded_tls). */
    .fastecc : ALIGN(4)
    {
        *(.text.*10field_impl6fe_*)
        *(.text.*16point_arithmetic*)
        *(.text.*10primeorder10projective*3ops5arith3Mul*)
    } >FLASH AT>FLASH

    .slow : ALIGN(4)
    {
        *(.slow .slow.*)
        *(.text.*http_task_task* .text.*stream_task_task* .text.*_embassy_main_task*)
        *(.text.*12freenetradio6remote* .text.*12freenetradio8settings* .text.*12freenetradio5serve* .text.*12freenetradio5clock5query* .text.*12freenetradio5clock9sync_once* .text.*ntp_task_task*)
        /* Web server helpers, run once per request. */
        *(.text.*12freenetradio7respond* .text.*12freenetradio12preset_reply* .text.*12freenetradio11status_json*)
        *(.text.*12freenetradio10url_decode* .text.*12freenetradio8json_str* .text.*12freenetradio10form_field*)
        *(.text.*12freenetradio12accepts_html* .text.*12freenetradio6header*)
        *(.text.*6dhcpv4* .text.*6socket3dns*)
        /* TLS for https streams: the stream connection (fetch, Conn) and
         * the handshake crates (P-256, SHA-2, HKDF...). NOT moved: AES-GCM,
         * which decrypts every byte of the stream. From slow flash it took
         * ~25 us per byte, so each 4 KB record stalled the audio for 100 ms
         * (heard as stutter). P-256 runs only while connecting, when
         * nothing is playing. */
        *(.text.*5radio5fetch* .text.*5radio4Conn*)
        *(.text.*12embedded_tls* .rodata.*12embedded_tls*)
                *(.text.*5ecdsa* .text.*4sec1* .text.*3der* .text.*9const_oid* .text.*7rfc6979* .text.*9signature*)
        *(.text.*4p256* .text.*10primeorder* .text.*16elliptic_curve* .text.*11crypto_bigint*)
        *(.text.*4sha2* .text.*4hmac* .text.*4hkdf* .text.*6digest* .text.*12block_buffer*)
        *(.rodata.*4p256* .rodata.*4sha2*)
        . = ALIGN(4);
    } >SLOW AT>SLOW

    .text : ALIGN(4)
    {
        . = ALIGN(4);
        KEEP(*(SORT_NONE(.handle_reset)))
        *(.init.rust)
        *(.text .text.*)
        *(.highcode .highcode.*);
    } >FLASH AT>FLASH

    .rodata : ALIGN(4)
    {
        *(.srodata .srodata.*);
        *(.rodata .rodata.*);
        . = ALIGN(4);
    } >FLASH AT>FLASH

    .data : ALIGN(4)
    {
        _data_lma = LOADADDR(.data);
        PROVIDE(_data_vma = .);
        PROVIDE( __global_pointer$ = . + 0x800 );
        *(.sdata .sdata.* .sdata2 .sdata2.*);
        *(.data .data.*);
        . = ALIGN(4);
        PROVIDE( _edata = .);
    } >RAM AT>FLASH

    .bss (NOLOAD) : ALIGN(4)
    {
        PROVIDE( _sbss = .);
        *(.sbss .sbss.* .bss .bss.*);
        PROVIDE( _ebss = .);
    } >RAM

    .uninit (NOLOAD) : ALIGN(4)
    {
        *(.uninit .uninit.*);
    } >RAM

    /* Align `__sheap` and `_stack_end` pointers to 4 bytes */
    . = ALIGN(4);

    /* Place the heap start and stack end at the end of allocated RAM */
    PROVIDE(__sheap = .);
    PROVIDE(_stack_end = .);

    .got (INFO) :
    {
        KEEP(*(.got .got.*));
    }

    .eh_frame (INFO) : { KEEP(*(.eh_frame)) }
    .eh_frame_hdr (INFO) : { *(.eh_frame_hdr) }
}

INCLUDE assert-align.x
