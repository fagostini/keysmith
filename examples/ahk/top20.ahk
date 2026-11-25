; The 20 most common AutoHotkey snippet patterns, v1 and v2.
; `keysmith convert examples/ahk/top20.ahk` turns them into Luau.
#Requires AutoHotkey v2.0
#SingleInstance Force
SendMode "Input"

; 1. Hotkey that types text (v2)
^j::Send "My First Script"
; 2. Same in v1 syntax
^!s::Send, Sincerely,{Enter}John Smith
; 3. Remap
CapsLock::Esc
; 4. Swap keys (remap both ways)
RAlt::RCtrl
; 5. Launch a program
#n::Run "notepad.exe"
; 6. Open a website
#g::Run "https://www.google.com"
; 7. Simple hotstring
::btw::by the way
; 8. Immediate hotstring
:*:;date::2026-01-01
; 9. Hotstring with keys
::sig::Best regards,{Enter}Jane
; 10. Media keys
^!Up::Send "{Volume_Up}"
^!Down::Volume_Down
; 11. Shortcut that sends a shortcut
#c::Send "^c"
; 12. Several keys with repeat
F1::Send "{Home}+{End}{Del 2}"
; 13. Multi-line v2 block with Sleep
^!w:: {
    Send "^a"
    Sleep 100
    Send "^c"
}
; 14. Multi-line v1 block with return
^!e::
Send, Hello
Sleep, 200
Send, {Tab}World
return
; 15. Message box
F2::MsgBox "Hello from Keysmith"
; 16. Literal text
F3::SendText "100% {literal} ^!+#"
; 17. App-specific hotkeys (v2)
#HotIf WinActive("ahk_exe firefox.exe")
^+t::Send "^t"
#HotIf
; 18. App-specific hotkeys (v1)
#IfWinActive ahk_class kitty
F4::Send, ls{Enter}
#IfWinActive
; 19. if/else on the active window
F6:: {
    if WinActive("ahk_exe code.exe")
        Send "^s"
    else
        Send "{F5}"
}
; 20. Passthrough modifiers and side-specific keys
~<^>!F7::ToolTip "Hi"

; Not convertible (each becomes a TODO line):
a & b::MsgBox "custom combination"
^r::Reload
