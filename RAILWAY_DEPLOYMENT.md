# Deploying to Railway

This document explains how to deploy the MYS Faucet to Railway.

## Deployment Steps

1. **Create a Railway Account**
   - Sign up at [railway.app](https://railway.app/)

2. **Create a New Project**
   - Click "New Project"
   - Select "Deploy from GitHub repo"
   - Connect to your GitHub account and select the repository

3. **Configure Environment Variables** (if needed)
   - In your project dashboard, go to the "Variables" tab
   - Add any required environment variables:
     - `CLOUDFLARE_TURNSTILE_URL` (if using authentication)
     - `TURNSTILE_SECRET_KEY` (if using authentication)
     - `DISCORD_BOT_PWD` (if using Discord integration)

4. **Deploy**
   - Railway will automatically detect the Dockerfile and build/deploy your application
   - Monitor the deployment in the "Deployments" tab

## DNS Configuration

To point your domain to the Railway-hosted faucet:

1. **Get your Railway domain**
   - From your project dashboard, note the auto-generated domain (e.g., `your-app-name.railway.app`)

2. **Set up DNS for your custom domain**
   - Add a CNAME record in your DNS provider:
     - Name: `faucet` (or whatever subdomain you want)
     - Value: `your-app-name.railway.app`
   - Example: `faucet.mysocial.network CNAME your-app-name.railway.app`

3. **Add your custom domain in Railway**
   - In your project settings, go to "Settings"
   - Under "Domains", add your custom domain (e.g., `faucet.mysocial.network`)
   - Railway will provide SSL certificates automatically

## Troubleshooting

- **Build Errors**: Check the build logs in Railway dashboard
- **Runtime Errors**: Check the logs in the "Deployments" tab
- **DNS Issues**: Allow up to 24-48 hours for DNS changes to propagate

## Testing

To test if your deployment is working:

```bash
# Health check
curl https://your-domain.com/health

# Request gas tokens
curl -X POST https://your-domain.com/gas \
  -H "Content-Type: application/json" \
  -d '{"recipient":"your-address-here"}'
``` 