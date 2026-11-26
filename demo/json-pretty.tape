# Render from the repository root with: python demo/render.py demo/json-pretty.tape
Output demo/json-pretty.gif
Source demo/setup.tape

Hide
Type "cd ../realworld && clear"
Enter
Sleep 300ms
Show

Type "byteseek -p -e '$.dependencies.react' -e '$.devDependencies.typescript' packages"
Enter
Sleep 1500ms
Screenshot docs/json-pretty.png
Sleep 500ms
