# Render from the repository root with: python demo/render.py demo/toml-pretty.tape
Output demo/toml-pretty.gif
Source demo/setup.tape

Hide
Type "cd ../realworld/vendor && clear"
Enter
Sleep 300ms
Show

Type `byteseek -p -e '$.package.rust-version' -e '$.package.edition == "2018"' -e '$.dependencies.tokio.version'`
Enter
Sleep 1500ms
Screenshot docs/toml-pretty.png
Sleep 500ms
