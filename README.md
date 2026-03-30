# rust-base64-resolver-gen

> Generate image endpoints by posting base64 image data to a temporary URL.

## Overview

`rust-base64-resolver-gen` is a Rust-based web service that allows users to upload base64-encoded images and retrieve them via unique URLs. This project is designed to facilitate the handling of image data in web applications, making it easy to generate temporary URLs for images.

## Features

- Accepts base64-encoded image data via a POST request.
- Generates a unique URL for each uploaded image.
- Allows retrieval of images using the generated URLs.
- Simple and efficient implementation using Actix-web.

## Prerequisites

- Rust (version 1.50 or later)
- Cargo (comes with Rust)
- Git (for cloning the repository)

## Installation

1. **Clone the Repository**

Open your terminal and run the following command to clone the repository:

```bash
git clone https://github.com/Tomato6966/rust-base64-resolver-gen.git
```

2. **Navigate to the Project Directory**

Change into the project directory:

```bash
cd rust-base64-resolver-gen
```

Now rename `example.config.toml` to `config.toml` + fill in the values

3. **Build the Project**

Use Cargo to build the project:

```bash
cargo build
```

4. **Run the Server**

Start the server with the following command:

```bash
cargo run
```

The server will start on `http://127.0.0.1:8080`.

## Usage

### Uploading an Image

To upload a base64-encoded image, send a POST request to the `/image` endpoint with a JSON body containing the base64 string. You can use tools like `curl` or Postman for testing.

**Example using `curl`:**

```bash
curl -X POST http://127.0.0.1:3555/image \
-H "Content-Type: application/json" \
-d '{"base64": "your_base64_encoded_image_here"}'
```
Example Url Encoded:
```bash
curl -X POST http://127.0.0.1:3555/image \
-H "Content-Type: application/x-www-form-urlencoded" \
-d "base64=your_base64_encoded_image_here"
```

**Response:**

The server will respond with a URL to access the uploaded image:

```json
{
  "urlPath": "/image/{uuid}"
}
```

### Retrieving an Image

To retrieve the uploaded image, send a GET request to the URL provided in the response.

**Example:**

```bash
curl -X GET http://127.0.0.1:8080/image/{uuid}
```

Replace `{uuid}` with the actual UUID returned from the upload response.

## Tutorial

### Step 1: Start the Server

Make sure your server is running by executing `cargo run` in the project directory.

### Step 2: Upload an Image

1. Convert your image to a base64 string. You can use online tools or libraries in various programming languages to do this.
2. Use the `curl` command provided above to upload your base64 string to the server.

### Step 3: Access the Image

After successfully uploading the image, you will receive a URL. Use this URL to access the image in your browser or through another HTTP client.

### Example

1. **Upload an Image:**

```bash
curl -X POST http://127.0.0.1:8080/image \
-H "Content-Type: application/json" \
-d '{"base64": "iVBORw0KGgoAAAANSUhEUgAAAAUA..."}'
```

**Response:**

```json
{
   "urlPath": "/image/4f430c6f-c2a5-4a66-84ca-12939dc6f172"
}
```

2. **Retrieve the Image:**

```bash
curl -X GET http://127.0.0.1:8080/image/4f430c6f-c2a5-4a66-84ca-12939dc6f172
```

The image will be returned in the response. That means you can also just open it in the browser.

## Contributing

Contributions are welcome! If you have suggestions for improvements or new features, feel free to open an issue or submit a pull request.

## License

This project is licensed under the MIT License. See the [LICENSE](LICENSE) file for details.

## Acknowledgments

- [Actix-web](https://actix.rs/) - The web framework used for this project.
- [Base64](https://crates.io/crates/base64) - The library used for encoding and decoding base64 data.
```

### Notes:
- Replace `"your_base64_encoded_image_here"` with an actual base64 string when testing.
- Ensure that the UUID in the example matches the format returned by your server.
- You can customize the README further based on your project's specific features or requirements.


You can visiualize the images easily with Postman with this POST-Request:
```js
function constructVisualizerPayload() {
    var response = pm.response.json();
    return { url: pm.variables.get("RUST_IMAGE_URL") + response.urlPath };
}

pm.visualizer.set(`<p><a href="{{url}}" target="_blank" style="color: white;">{{url}}</a></p><img src="{{url}}" alt="Generated Image" />`, constructVisualizerPayload());
```
