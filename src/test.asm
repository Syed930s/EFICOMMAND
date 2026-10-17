; hi.asm - 64-bit .COM tool for EFICOMMAND
; run  :  X:\> hi          (or: start hi.com)
;
; entry ABI: RCX = *ComApi, return code in RAX, Microsoft x64 calling convention
;   ComApi + 32 = write(ptr, len)

format binary as 'com'
use64

start:
        push    rbx                     ; callee-saved, we use it
        sub     rsp, 32                 ; shadow space (keeps RSP 16-byte aligned)
        mov     rbx, rcx                ; rbx = api table

        lea     rcx, [msg]              ; RIP-relative -> position independent
        mov     edx, msg_len
        call    qword [rbx + 32]        ; api->write(msg, msg_len)

        xor     eax, eax                ; exit code 0
        add     rsp, 32
        pop     rbx
        ret

msg:    db 'Hi', 10
msg_len = $ - msg
