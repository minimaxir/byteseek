# Render from the repository root with: python demo/render.py demo/yaml-pretty.tape
Output demo/yaml-pretty.gif
Source demo/setup.tape

Hide
Type "cd ../realworld && clear"
Enter
Sleep 300ms
Show

Type `byteseek -p -e '$.permissions.contents == "write"' -e secrets. -e @v3 .github/workflows`
Enter
Sleep 1500ms
Screenshot docs/yaml-pretty.png
Sleep 500ms
