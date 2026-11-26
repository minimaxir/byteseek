# Render from the repository root with: python demo/render.py demo/predicates.tape
Output demo/predicates.gif
Source demo/setup.tape

Hide
Type "cd ../realworld && clear"
Enter
Sleep 300ms
Show

Type `byteseek -p -e '$.restartCount > 5 AND $.ready == false' -e '$.exitCode == 137' pods.json`
Enter
Sleep 1500ms
Type `byteseek -p '$.finishedAt > "2026-11-21T09:05:00Z" AND $.reason == "OOMKilled"' pods.json`
Enter
Sleep 1500ms
Screenshot docs/predicates.png
Sleep 500ms
