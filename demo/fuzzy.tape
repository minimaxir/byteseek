# Render from the repository root with: python demo/render.py demo/fuzzy.tape
Output demo/fuzzy.gif
Source demo/setup.tape

Type "byteseek -p -k 1 timeout logs"
Enter
Sleep 1500ms
Type "byteseek -p --query 'ERROR AND payments AND NOT 502' logs"
Enter
Sleep 1500ms
Screenshot docs/fuzzy.png
Sleep 500ms
