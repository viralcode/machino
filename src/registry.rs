//! HTTP-based package registry client.

#[cfg(feature = "registry")]
use ureq;

/// Upload a package to the registry.
#[cfg(feature = "registry")]
pub fn upload_package(registry_url: &str, package_path: &std::path::Path, token: &str) -> Result<String, String> {
    use std::fs;
    
    // Read package manifest
    let manifest_path = package_path.join("machino.pkg");
    let manifest = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("failed to read manifest: {}", e))?;
    
    // Create tarball of package
    let tarball = create_tarball(package_path)?;
    
    // Upload to registry
    let response = ureq::post(&format!("{}/api/v1/packages", registry_url))
        .set("Authorization", &format!("Bearer {}", token))
        .set("Content-Type", "application/x-tar")
        .send_bytes(&tarball)
        .map_err(|e| format!("upload failed: {}", e))?;
    
    if response.status() == 201 {
        Ok("package uploaded successfully".to_string())
    } else {
        Err(format!("upload failed with status {}", response.status()))
    }
}

/// Download a package from the registry.
#[cfg(feature = "registry")]
pub fn download_package(registry_url: &str, name: &str, version: &str, dest: &std::path::Path) -> Result<(), String> {
    let url = format!("{}/api/v1/packages/{}/{}", registry_url, name, version);
    
    let response = ureq::get(&url)
        .call()
        .map_err(|e| format!("download failed: {}", e))?;
    
    if response.status() != 200 {
        return Err(format!("package not found (status {})", response.status()));
    }
    
    // Read tarball
    let mut tarball = Vec::new();
    response.into_reader()
        .read_to_end(&mut tarball)
        .map_err(|e| format!("failed to read response: {}", e))?;
    
    // Extract to destination
    extract_tarball(&tarball, dest)?;
    
    Ok(())
}

#[cfg(feature = "registry")]
fn create_tarball(package_path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::fs;
    use std::io::Write;
    
    let mut tarball = Vec::new();
    
    // Simple tar format: just concatenate files with headers
    for entry in fs::read_dir(package_path)
        .map_err(|e| format!("failed to read directory: {}", e))? 
    {
        let entry = entry.map_err(|e| format!("failed to read entry: {}", e))?;
        let path = entry.path();
        
        if path.is_file() && path.extension().map_or(false, |e| e == "mno" || e == "pkg") {
            let contents = fs::read(&path)
                .map_err(|e| format!("failed to read file: {}", e))?;
            
            // Write simple header: filename length, filename, content length, content
            let filename = path.file_name().unwrap().to_string_lossy();
            tarball.write_all(&(filename.len() as u32).to_le_bytes()).unwrap();
            tarball.write_all(filename.as_bytes()).unwrap();
            tarball.write_all(&(contents.len() as u32).to_le_bytes()).unwrap();
            tarball.write_all(&contents).unwrap();
        }
    }
    
    Ok(tarball)
}

#[cfg(feature = "registry")]
fn extract_tarball(tarball: &[u8], dest: &std::path::Path) -> Result<(), String> {
    use std::fs;
    use std::io::Write;
    
    let mut pos = 0;
    
    while pos < tarball.len() {
        if pos + 4 > tarball.len() {
            break;
        }
        
        // Read filename length
        let name_len = u32::from_le_bytes([
            tarball[pos], tarball[pos+1], tarball[pos+2], tarball[pos+3]
        ]) as usize;
        pos += 4;
        
        if pos + name_len > tarball.len() {
            return Err("corrupted tarball: invalid name length".to_string());
        }
        
        // Read filename
        let filename = String::from_utf8_lossy(&tarball[pos..pos+name_len]).to_string();
        pos += name_len;
        
        if pos + 4 > tarball.len() {
            return Err("corrupted tarball: missing content length".to_string());
        }
        
        // Read content length
        let content_len = u32::from_le_bytes([
            tarball[pos], tarball[pos+1], tarball[pos+2], tarball[pos+3]
        ]) as usize;
        pos += 4;
        
        if pos + content_len > tarball.len() {
            return Err("corrupted tarball: invalid content length".to_string());
        }
        
        // Read and write content
        let content = &tarball[pos..pos+content_len];
        let file_path = dest.join(&filename);
        let mut file = fs::File::create(&file_path)
            .map_err(|e| format!("failed to create file: {}", e))?;
        file.write_all(content)
            .map_err(|e| format!("failed to write file: {}", e))?;
        
        pos += content_len;
    }
    
    Ok(())
}

#[cfg(not(feature = "registry"))]
pub fn upload_package(_registry_url: &str, _package_path: &std::path::Path, _token: &str) -> Result<String, String> {
    Err("registry support not enabled (compile with --features registry)".to_string())
}

#[cfg(not(feature = "registry"))]
#[allow(dead_code)]
pub fn download_package(_registry_url: &str, _name: &str, _version: &str, _dest: &std::path::Path) -> Result<(), String> {
    Err("registry support not enabled (compile with --features registry)".to_string())
}
