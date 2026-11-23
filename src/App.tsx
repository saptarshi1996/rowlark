import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

function App() {
  const [coreVersion, setCoreVersion] = useState<string | null>(null);

  useEffect(() => {
    invoke<string>("core_version").then(setCoreVersion);
  }, []);

  return (
    <main>
      <h1>Rowlark</h1>
      <p>Core version: {coreVersion ?? "loading…"}</p>
    </main>
  );
}

export default App;
