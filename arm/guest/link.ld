ENTRY(_start)

SECTIONS
{
    . = 0x200000;
    .text : {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }
    .rodata : { *(.rodata .rodata.*) }
    .data : { *(.data .data.*) }
    . = ALIGN(4096);
    __bss_start = .;
    .bss : { *(.bss .bss.* COMMON) }
    . = ALIGN(16);
    __bss_end = .;
    . = . + 0x10000;
    . = ALIGN(16);
    __stack_top = .;
    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) }
}
