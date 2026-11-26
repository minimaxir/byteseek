# Settings shared by every demo tape, with psnek's look. Each tape records taller than it
# needs, and demo/render.py crops the screenshot to the text.
Set Theme "Tomorrow Night Bright"
Set FontFamily "JetBrains Mono"
Set FontSize 16
Set Width 1200
Set Height 900
Set Padding 32
Set LineHeight 1.0
Set LetterSpacing 0
Set TypingSpeed 0

Env COLORTERM "truecolor"
# byteseek writes plain text for CI and AI agents; these recordings are for people.
Env FORCE_COLOR "1"

Hide
Type "export PATH=$PWD/target/release:$PATH PS1='$ ' && cd demo/project && clear"
Enter
Sleep 500ms
Show
