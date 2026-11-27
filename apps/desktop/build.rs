fn main() {
    slint_build::compile("ui/app.slint").expect("failed to compile Slint UI");
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("icons/icon.ico");
        res.set("ProductName", "SlipStream");
        res.set("FileDescription", "SlipStream Download Manager");
        res.set("CompanyName", "Mohamed Sayed");
        res.set("LegalCopyright", "Copyright (c) 2026 Mohamed Sayed");
        res.compile().expect("failed to compile Windows resources");
    }
}
